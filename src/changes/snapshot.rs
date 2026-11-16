//! Change scan for directory snapshots: compare the view against the fork manifest, using
//! ctime past the fork instant as the fast "touched" signal.
//!
//! The scan walks the view once, reporting entries that differ from their manifest row (or
//! have no row), then reports manifest rows the walk never saw as deletions.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::content::{compute_line_diff, count_lines, is_binary};
use super::filter::ChangeFilter;
use super::manifest::{
    self, encode_path, written_after_fork, Manifest, ManifestKind, ManifestRow, ManifestStatus,
};
use super::{ChangeKind, FileChange, OldContent};
use crate::backend::ChangeCount;
use crate::error::Result;
use crate::util::walk::{Entry, Step};
use crate::workspace::Workspace;

/// Changes in a snapshot workspace's view relative to its fork manifest. Without a readable
/// manifest only entries written after the fork are reported, and deletions are not.
pub fn collect_clone_changes(
    workspace: &Workspace,
    include_ignored: bool,
    need_line_counts: bool,
) -> Result<Vec<FileChange>> {
    let filter = (!include_ignored).then(|| ChangeFilter::build(&workspace.base));
    let baseline = load_baseline(workspace, include_ignored)?.ok().flatten();
    collect(workspace, filter, baseline, need_line_counts)
}

/// How many changes `ls` shows for a snapshot workspace with a `Ready` manifest. A manifest
/// found unreadable here reports what recovery made of it (degraded, or rebuilding) rather
/// than a count that cannot see deletions.
pub fn count_clone_changes(workspace: &Workspace, include_ignored: bool) -> Result<ChangeCount> {
    let filter = (!include_ignored).then(|| ChangeFilter::build(&workspace.base));
    match load_baseline(workspace, include_ignored)? {
        Ok(baseline) => Ok(ChangeCount::Ready(
            collect(workspace, filter, baseline, false)?.len(),
        )),
        Err(recovered) => Ok(recovered),
    }
}

/// Changes that can move between the workspace and its base (everything but runtime and
/// internal paths). `node_modules` and `.git` are not walked at all.
pub fn collect_transferable_clone_changes(workspace: &Workspace) -> Result<Vec<FileChange>> {
    let baseline = load_baseline(workspace, true)?.ok().flatten();
    collect(
        workspace,
        Some(ChangeFilter::transferable_only()),
        baseline,
        false,
    )
}

fn collect(
    workspace: &Workspace,
    filter: Option<ChangeFilter>,
    baseline: Option<Manifest>,
    need_line_counts: bool,
) -> Result<Vec<FileChange>> {
    let scan = CloneScan {
        workspace,
        filter,
        baseline,
        need_line_counts,
    };

    let walk = scan.walk_view()?;
    let mut changes = walk.changes;
    changes.extend(scan.deletions(&walk.seen, &walk.pruned));
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

/// Open a `Ready` manifest (`Ok(None)` for a manifest that is not ready). If it cannot be
/// read, run [`manifest::change_count`] for its recovery side effects (degrade, or schedule a
/// rebuild) and return its verdict, so the scan goes without a baseline.
fn load_baseline(
    workspace: &Workspace,
    include_ignored: bool,
) -> Result<std::result::Result<Option<Manifest>, ChangeCount>> {
    if workspace.manifest_status != ManifestStatus::Ready {
        return Ok(Ok(None));
    }
    match Manifest::open(&workspace.name) {
        Ok(rows) => Ok(Ok(Some(rows))),
        Err(_) => Ok(Err(manifest::change_count(workspace, include_ignored)?)),
    }
}

struct CloneScan<'a> {
    workspace: &'a Workspace,
    /// `None` when ignored paths are included.
    filter: Option<ChangeFilter>,
    /// The fork manifest, sorted by encoded path; `None` when it is unavailable. With a
    /// baseline, an entry without a row is new; without one, only a post-fork ctime marks it
    /// as changed.
    baseline: Option<Manifest>,
    need_line_counts: bool,
}

/// One entry of the view walk.
enum Seen {
    /// An invisible directory, not descended into.
    Pruned,
    /// An invisible entry.
    Hidden,
    /// A visible entry, with its change if it changed.
    Visible(Option<FileChange>),
}

/// What the view walk found.
struct ViewWalk {
    changes: Vec<FileChange>,
    /// Every path the walk visited, visible or not, encoded as in the manifest.
    seen: HashSet<String>,
    /// Invisible directories the walk did not descend into, encoded as in the manifest.
    pruned: Vec<String>,
}

impl CloneScan<'_> {
    fn baseline_known(&self) -> bool {
        self.baseline.is_some()
    }

    /// The manifest row of `relative`, if there is a manifest and it has one.
    fn row(&self, relative: &Path) -> Option<ManifestRow> {
        let baseline = self.baseline.as_ref()?;
        baseline.get(&encode_path(relative)).map(|row| row.to_row())
    }

    fn is_visible(&self, path: &Path, is_dir: bool) -> bool {
        self.filter
            .as_ref()
            .is_none_or(|filter| filter.is_visible(path, is_dir))
    }

    /// Walk the view on several threads, skipping invisible directories, and report every
    /// visible entry that changed since the fork.
    fn walk_view(&self) -> Result<ViewWalk> {
        let root = &self.workspace.view_path;
        let found = crate::util::walk::parallel_with(root, |relative, path, metadata| {
            let is_dir = metadata.is_dir();
            if !self.is_visible(relative, is_dir) {
                return Ok(if is_dir {
                    Step::Prune(Some(Seen::Pruned))
                } else {
                    Step::Descend(Some(Seen::Hidden))
                });
            }
            let row = self.row(relative);
            // Directories are reported only when they replace a non-directory entry.
            let reportable = !is_dir
                || row
                    .as_ref()
                    .is_some_and(|row| row.kind != ManifestKind::Dir);
            let change = (reportable && self.changed(path, metadata, row.as_ref()))
                .then(|| self.change(relative.to_path_buf(), Some(path), row.as_ref()));
            Ok(Step::Descend(Some(Seen::Visible(change))))
        })?;

        let mut seen = HashSet::new();
        let mut pruned = Vec::new();
        let mut changes = Vec::new();
        for (path, item) in found {
            match item {
                Seen::Pruned => {
                    // The directory itself was seen; only what is below it was not walked.
                    let encoded = encode_path(&path);
                    seen.insert(encoded.clone());
                    pruned.push(encoded);
                }
                Seen::Hidden | Seen::Visible(None) => {
                    seen.insert(encode_path(&path));
                }
                Seen::Visible(Some(change)) => {
                    seen.insert(encode_path(&path));
                    changes.push(change);
                }
            }
        }
        Ok(ViewWalk {
            changes,
            seen,
            pruned,
        })
    }

    fn changed(&self, path: &Path, metadata: &Entry, row: Option<&ManifestRow>) -> bool {
        let written = written_after_fork(metadata, self.workspace.fork_time_ns);
        match row {
            Some(row) => row.dirty || written || !row.matches(path, metadata),
            None => self.baseline_known() || written,
        }
    }

    /// Manifest rows the walk did not see. A missing directory is reported once, not once
    /// per descendant; rows under pruned or invisible paths are not reported at all.
    ///
    /// The rows are sorted by encoded path, so each pruned directory (`node_modules`, most of
    /// a JavaScript project) is skipped as one range instead of row by row.
    fn deletions(&self, seen: &HashSet<String>, pruned: &[String]) -> Vec<FileChange> {
        let Some(baseline) = &self.baseline else {
            return Vec::new();
        };
        let mut skip: Vec<(usize, usize)> = pruned
            .iter()
            .map(|dir| {
                (
                    baseline.lower_bound(&format!("{dir}/")),
                    baseline.end_of_subtree(dir),
                )
            })
            .collect();
        skip.sort_unstable();
        let mut missing: Vec<(PathBuf, ManifestRow)> = Vec::new();
        let mut index = 0;
        let mut ranges = skip.into_iter().peekable();
        while index < baseline.len() {
            while ranges.peek().is_some_and(|&(_, end)| end <= index) {
                ranges.next();
            }
            if let Some(&(start, end)) = ranges.peek() {
                if start <= index {
                    index = end;
                    continue;
                }
            }
            let row = baseline.row(index);
            index += 1;
            if seen.contains(row.path) {
                continue;
            }
            let Ok(path) = manifest::decode_path(row.path) else {
                continue;
            };
            if self.is_visible(&path, row.kind == ManifestKind::Dir) {
                missing.push((path, row.to_row()));
            }
        }
        let missing_directories: BTreeSet<&Path> = missing
            .iter()
            .filter(|(_, row)| row.kind == ManifestKind::Dir)
            .map(|(path, _)| path.as_path())
            .collect();
        let under_missing_directory = |path: &Path| {
            path.ancestors()
                .skip(1)
                .any(|ancestor| missing_directories.contains(ancestor))
        };
        missing
            .iter()
            .filter(|(path, _)| !under_missing_directory(path))
            .map(|(path, row)| self.change(path.clone(), None, Some(row)))
            .collect()
    }

    /// Build the change for `path`. `current` is the entry in the view (`None` when deleted)
    /// and `row` its manifest baseline, if any.
    fn change(
        &self,
        path: PathBuf,
        current: Option<&Path>,
        row: Option<&ManifestRow>,
    ) -> FileChange {
        let base_path = self.workspace.base.join(&path);
        let kind = match (current, row) {
            (None, _) => ChangeKind::Deleted,
            // Without a manifest, a written file the base also has is an edit of it.
            (Some(_), None)
                if !self.baseline_known() && fs::symlink_metadata(&base_path).is_ok() =>
            {
                ChangeKind::Modified
            }
            (Some(_), None) => ChangeKind::Added,
            // A dirty row may record a file created after the fork; if the base never had
            // it, it is an addition.
            (Some(_), Some(row)) if row.dirty && fs::symlink_metadata(&base_path).is_err() => {
                ChangeKind::Added
            }
            (Some(_), Some(_)) => ChangeKind::Modified,
        };
        let old_content = match row {
            Some(row) if row.dirty => OldContent::AncestorUnknown,
            Some(row) if base_matches(&base_path, row) => OldContent::Available,
            Some(_) => OldContent::BaseAdvanced,
            None if kind == ChangeKind::Added => OldContent::Available,
            None => OldContent::AncestorUnknown,
        };
        let change = FileChange {
            old_content,
            ..FileChange::new(path, kind)
        };

        let Some(current) = current.filter(|_| self.need_line_counts) else {
            return change;
        };
        if current.is_symlink() {
            return change;
        }
        if is_binary(current) {
            return FileChange {
                is_binary: true,
                ..change
            };
        }
        if !current.is_file() {
            return change;
        }
        match (kind, old_content) {
            (ChangeKind::Added, _) => FileChange {
                additions: Some(count_lines(current)),
                ..change
            },
            (ChangeKind::Modified, OldContent::Available) => {
                let (additions, deletions) = compute_line_diff(&base_path, current);
                FileChange {
                    additions: Some(additions),
                    deletions: Some(deletions),
                    ..change
                }
            }
            _ => change,
        }
    }
}

/// Whether the base entry still matches the fork-time manifest row, i.e. the base still
/// holds the ancestor content.
fn base_matches(base_path: &Path, row: &ManifestRow) -> bool {
    fs::symlink_metadata(base_path)
        .is_ok_and(|metadata| row.matches(base_path, &crate::util::walk::Entry::of(&metadata)))
}
