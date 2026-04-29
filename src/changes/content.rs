//! Byte- and line-level comparison of a changed file against its base copy.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use similar::{ChangeTag, TextDiff};

use super::filter::ChangeFilter;
use super::{ChangeKind, FileChange, OldContent};

pub fn is_binary(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 8192];
    let Ok(n) = file.read(&mut buf) else {
        return false;
    };
    buf[..n].contains(&0)
}

pub(super) fn compute_line_diff(old: &Path, new: &Path) -> (usize, usize) {
    let old_content = fs::read_to_string(old).unwrap_or_default();
    let new_content = fs::read_to_string(new).unwrap_or_default();

    let diff = TextDiff::from_lines(&old_content, &new_content);
    let mut additions = 0usize;
    let mut deletions = 0usize;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }

    (additions, deletions)
}

pub(super) fn files_differ(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let (Ok(meta_a), Ok(meta_b)) = (fs::symlink_metadata(a), fs::symlink_metadata(b)) else {
        return true;
    };
    if meta_a.file_type().is_symlink() || meta_b.file_type().is_symlink() {
        return meta_a.file_type().is_symlink() != meta_b.file_type().is_symlink()
            || fs::read_link(a).ok() != fs::read_link(b).ok();
    }
    if meta_a.is_dir() != meta_b.is_dir()
        || meta_a.len() != meta_b.len()
        || meta_a.mode() & 0o7777 != meta_b.mode() & 0o7777
    {
        return true;
    }

    let (Ok(mut fa), Ok(mut fb)) = (fs::File::open(a), fs::File::open(b)) else {
        return true;
    };
    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];
    loop {
        let na = match fa.read(&mut buf_a) {
            Ok(n) => n,
            Err(_) => return true,
        };
        let nb = match fb.read(&mut buf_b) {
            Ok(n) => n,
            Err(_) => return true,
        };
        if na != nb || buf_a[..na] != buf_b[..nb] {
            return true;
        }
        if na == 0 {
            return false;
        }
    }
}

pub(super) fn classify_file(
    rel_path: PathBuf,
    upper_path: &Path,
    base_path: &Path,
    need_line_counts: bool,
) -> Option<FileChange> {
    if fs::symlink_metadata(base_path).is_ok() {
        if need_line_counts {
            let symlink = upper_path.is_symlink() || base_path.is_symlink();
            let binary = !symlink && is_binary(upper_path);
            let (additions, deletions) = if binary || symlink {
                if !files_differ(base_path, upper_path) {
                    return None;
                }
                (None, None)
            } else {
                let (a, d) = compute_line_diff(base_path, upper_path);
                if a == 0 && d == 0 && !files_differ(base_path, upper_path) {
                    return None;
                }
                (Some(a), Some(d))
            };
            Some(FileChange {
                class: ChangeFilter::class(&rel_path),
                path: rel_path,
                kind: ChangeKind::Modified,
                additions,
                deletions,
                is_binary: binary,
                old_content: OldContent::Available,
            })
        } else {
            if !files_differ(base_path, upper_path) {
                return None;
            }
            Some(FileChange {
                class: ChangeFilter::class(&rel_path),
                path: rel_path,
                kind: ChangeKind::Modified,
                additions: None,
                deletions: None,
                is_binary: false,
                old_content: OldContent::Available,
            })
        }
    } else if need_line_counts {
        let binary = !upper_path.is_symlink() && is_binary(upper_path);
        let additions = if binary || upper_path.is_symlink() {
            None
        } else {
            Some(
                fs::read_to_string(upper_path)
                    .unwrap_or_default()
                    .lines()
                    .count(),
            )
        };
        Some(FileChange {
            class: ChangeFilter::class(&rel_path),
            path: rel_path,
            kind: ChangeKind::Added,
            additions,
            deletions: None,
            is_binary: binary,
            old_content: OldContent::Available,
        })
    } else {
        Some(FileChange {
            class: ChangeFilter::class(&rel_path),
            path: rel_path,
            kind: ChangeKind::Added,
            additions: None,
            deletions: None,
            is_binary: false,
            old_content: OldContent::Available,
        })
    }
}
