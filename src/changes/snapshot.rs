//! Change scan for directory snapshots: compare the view against the fork manifest, using
//! ctime past the fork instant as the fast "touched" signal.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use super::content::{compute_line_diff, is_binary};
use super::filter::ChangeFilter;
use super::manifest::{self, ManifestKind, ManifestRow, ManifestStatus};
use super::{ChangeKind, FileChange, OldContent};
use crate::error::{ForkspaceError, Result};
use crate::util::WalkDirExt;
use crate::workspace::Workspace;

fn metadata_timestamp_ns(metadata: &fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
}

fn metadata_ctime_ns(metadata: &fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec())
}

fn metadata_kind(metadata: &fs::Metadata) -> ManifestKind {
    if metadata.file_type().is_symlink() {
        ManifestKind::Symlink
    } else if metadata.is_dir() {
        ManifestKind::Dir
    } else {
        ManifestKind::File
    }
}

fn base_matches_manifest(base: &Path, row: &ManifestRow) -> bool {
    let Ok(metadata) = fs::symlink_metadata(base) else {
        return false;
    };
    if metadata_kind(&metadata) != row.kind
        || metadata.len() != row.size
        || (row.kind != ManifestKind::Symlink && metadata_timestamp_ns(&metadata) != row.mtime_ns)
        || (row.mode != 0 && metadata.mode() & 0o7777 != row.mode)
    {
        return false;
    }
    if row.kind == ManifestKind::Symlink {
        return fs::read_link(base)
            .ok()
            .is_some_and(|target| Some(manifest::encode_path(&target)) == row.target);
    }
    true
}

fn clone_change(
    workspace: &Workspace,
    path: PathBuf,
    current_path: Option<&Path>,
    baseline: Option<&ManifestRow>,
    need_line_counts: bool,
) -> FileChange {
    let base_path = workspace.base.join(&path);
    let kind = if current_path.is_none() {
        ChangeKind::Deleted
    } else if baseline.is_none()
        || baseline.is_some_and(|row| row.dirty && fs::symlink_metadata(&base_path).is_err())
    {
        ChangeKind::Added
    } else {
        ChangeKind::Modified
    };
    let old_content = match baseline {
        Some(row) if row.dirty => OldContent::AncestorUnknown,
        Some(row) if base_matches_manifest(&base_path, row) => OldContent::Available,
        Some(_) => OldContent::BaseAdvanced,
        None if matches!(kind, ChangeKind::Added) => OldContent::Available,
        None => OldContent::AncestorUnknown,
    };
    let binary =
        need_line_counts && current_path.is_some_and(|path| !path.is_symlink() && is_binary(path));
    let (additions, deletions) = match (&kind, current_path, old_content, need_line_counts) {
        (_, _, _, false) => (None, None),
        (ChangeKind::Added, Some(path), _, true)
            if !binary && !path.is_symlink() && path.is_file() =>
        {
            (
                Some(fs::read_to_string(path).unwrap_or_default().lines().count()),
                None,
            )
        }
        (ChangeKind::Modified, Some(path), OldContent::Available, true)
            if !binary && !path.is_symlink() && path.is_file() =>
        {
            let (additions, deletions) = compute_line_diff(&base_path, path);
            (Some(additions), Some(deletions))
        }
        _ => (None, None),
    };
    FileChange {
        class: ChangeFilter::class(&path),
        path,
        kind,
        additions,
        deletions,
        is_binary: binary,
        old_content,
    }
}

pub fn collect_clone_changes(
    workspace: &Workspace,
    include_ignored: bool,
    need_line_counts: bool,
) -> Result<Vec<FileChange>> {
    let filter = (!include_ignored).then(|| ChangeFilter::build(&workspace.base));
    let baseline = if workspace.manifest_status == ManifestStatus::Ready {
        let streamed = manifest::stream(&workspace.name).and_then(|rows| {
            rows.map(|row| {
                let row = row?;
                Ok((manifest::decode_path(&row.path)?, row))
            })
            .collect::<Result<HashMap<_, _>>>()
        });
        match streamed {
            Ok(rows) => Some(rows),
            Err(_) => {
                let _ = manifest::change_count(workspace, include_ignored)?;
                None
            }
        }
    } else {
        None
    };
    let baseline_known = baseline.is_some();
    let baseline = baseline.unwrap_or_default();
    let fork_time_ns = i128::try_from(workspace.fork_time_ns).unwrap_or(i128::MAX);
    let mut seen = BTreeSet::new();
    let mut pruned = BTreeSet::new();
    let root = &workspace.view_path;
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 || !entry.file_type().is_dir() || include_ignored {
                return true;
            }
            let Ok(relative) = entry.path().strip_prefix(root) else {
                return true;
            };
            let visible = filter
                .as_ref()
                .is_none_or(|filter| filter.is_visible(relative, true));
            if !visible {
                pruned.insert(relative.to_path_buf());
            }
            visible
        });
    let mut changes = Vec::new();
    for entry in walker.skip(1) {
        let entry = entry.walk_context(root)?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| ForkspaceError::StateFailed("clone scan escaped the workspace".to_string()))?
            .to_path_buf();
        seen.insert(relative.clone());
        let visible = filter
            .as_ref()
            .is_none_or(|filter| filter.is_visible(&relative, entry.file_type().is_dir()));
        if !visible {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|source| ForkspaceError::Io {
            context: format!("scan clone metadata for {}", entry.path().display()),
            source,
        })?;
        let baseline_row = baseline.get(&relative);
        let changed = match baseline_row {
            Some(row) => {
                row.dirty
                    || metadata_ctime_ns(&metadata) > fork_time_ns
                    || metadata_kind(&metadata) != row.kind
                    || metadata.len() != row.size
                    || (row.kind != ManifestKind::Symlink
                        && metadata_timestamp_ns(&metadata) != row.mtime_ns)
                    || (row.mode != 0 && metadata.mode() & 0o7777 != row.mode)
                    || (row.kind == ManifestKind::Symlink
                        && fs::read_link(entry.path()).ok().is_none_or(|target| {
                            Some(manifest::encode_path(&target)) != row.target
                        }))
            }
            None => baseline_known || metadata_ctime_ns(&metadata) > fork_time_ns,
        };
        if changed
            && (!metadata.is_dir() || baseline_row.is_some_and(|row| row.kind != ManifestKind::Dir))
        {
            changes.push(clone_change(
                workspace,
                relative,
                Some(entry.path()),
                baseline_row,
                need_line_counts,
            ));
        }
    }

    let missing_directories: BTreeSet<&Path> = baseline
        .iter()
        .filter(|(path, row)| {
            row.kind == ManifestKind::Dir
                && !seen.contains(*path)
                && !pruned.iter().any(|prefix| path.starts_with(prefix))
                && filter
                    .as_ref()
                    .is_none_or(|filter| filter.is_visible(path, true))
        })
        .map(|(path, _)| path.as_path())
        .collect();
    for (path, row) in &baseline {
        if seen.contains(path)
            || pruned.iter().any(|prefix| path.starts_with(prefix))
            || path
                .ancestors()
                .skip(1)
                .any(|ancestor| missing_directories.contains(ancestor))
            || filter
                .as_ref()
                .is_some_and(|filter| !filter.is_visible(path, row.kind == ManifestKind::Dir))
        {
            continue;
        }
        changes.push(clone_change(
            workspace,
            path.clone(),
            None,
            Some(row),
            need_line_counts,
        ));
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}
