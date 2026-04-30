//! Byte- and line-level comparison of a changed file against its base copy.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use similar::{ChangeTag, TextDiff};

use super::{ChangeKind, FileChange};
use crate::util::fs::{mode_bits, same_contents};

const SNIFF_LEN: usize = 8192;

/// Heuristic binary check: a NUL byte in the first 8 KiB. Unreadable files count as text.
pub fn is_binary(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; SNIFF_LEN];
    let Ok(n) = file.read(&mut buf) else {
        return false;
    };
    buf[..n].contains(&0)
}

/// Number of lines in a text file; unreadable or non-UTF-8 files count as empty.
pub(super) fn count_lines(path: &Path) -> usize {
    fs::read_to_string(path).unwrap_or_default().lines().count()
}

/// `(additions, deletions)` of a line diff from `old` to `new`.
pub(super) fn compute_line_diff(old: &Path, new: &Path) -> (usize, usize) {
    let old_content = fs::read_to_string(old).unwrap_or_default();
    let new_content = fs::read_to_string(new).unwrap_or_default();

    let mut additions = 0;
    let mut deletions = 0;
    for change in TextDiff::from_lines(&old_content, &new_content).iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }
    (additions, deletions)
}

/// Whether two entries differ in type, symlink target, size, permissions, or bytes.
/// Any error reading either side counts as a difference.
pub(super) fn files_differ(a: &Path, b: &Path) -> bool {
    let (Ok(meta_a), Ok(meta_b)) = (fs::symlink_metadata(a), fs::symlink_metadata(b)) else {
        return true;
    };
    let (link_a, link_b) = (
        meta_a.file_type().is_symlink(),
        meta_b.file_type().is_symlink(),
    );
    if link_a || link_b {
        return link_a != link_b || fs::read_link(a).ok() != fs::read_link(b).ok();
    }
    if meta_a.is_dir() != meta_b.is_dir()
        || meta_a.len() != meta_b.len()
        || mode_bits(&meta_a) != mode_bits(&meta_b)
    {
        return true;
    }
    !same_contents(a, b)
}

/// Classify `upper_path` (the new content) against `base_path`: `Added` when the base has no
/// entry, `Modified` when the two differ, and `None` when they are identical. Line counts
/// and binary detection are only computed when `need_line_counts` is set.
pub(super) fn classify_file(
    rel_path: PathBuf,
    upper_path: &Path,
    base_path: &Path,
    need_line_counts: bool,
) -> Option<FileChange> {
    if fs::symlink_metadata(base_path).is_err() {
        return Some(added_file(rel_path, upper_path, need_line_counts));
    }
    modified_file(rel_path, upper_path, base_path, need_line_counts)
}

fn added_file(rel_path: PathBuf, upper_path: &Path, need_line_counts: bool) -> FileChange {
    let change = FileChange::new(rel_path, ChangeKind::Added);
    if !need_line_counts || upper_path.is_symlink() {
        return change;
    }
    if is_binary(upper_path) {
        return FileChange {
            is_binary: true,
            ..change
        };
    }
    FileChange {
        additions: Some(count_lines(upper_path)),
        ..change
    }
}

fn modified_file(
    rel_path: PathBuf,
    upper_path: &Path,
    base_path: &Path,
    need_line_counts: bool,
) -> Option<FileChange> {
    let change = FileChange::new(rel_path, ChangeKind::Modified);
    if !need_line_counts {
        return files_differ(base_path, upper_path).then_some(change);
    }

    let symlink = upper_path.is_symlink() || base_path.is_symlink();
    if symlink || is_binary(upper_path) {
        return files_differ(base_path, upper_path).then_some(FileChange {
            is_binary: !symlink,
            ..change
        });
    }

    // An empty line diff can still hide a change (a chmod, or bytes that are not valid
    // UTF-8 and so read as empty), so confirm with a full comparison before dismissing it.
    let (additions, deletions) = compute_line_diff(base_path, upper_path);
    if additions == 0 && deletions == 0 && !files_differ(base_path, upper_path) {
        return None;
    }
    Some(FileChange {
        additions: Some(additions),
        deletions: Some(deletions),
        ..change
    })
}
