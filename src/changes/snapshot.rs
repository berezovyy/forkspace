//! Change scan for directory snapshots: compare the view against the fork manifest, using
//! ctime past the fork instant as the fast "touched" signal.
//!
//! The scan walks the view once, reporting entries that differ from their manifest row (or
//! have no row), then reports manifest rows the walk never saw as deletions.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use super::content::{compute_line_diff, count_lines, is_binary};
use super::filter::ChangeFilter;
use super::manifest::{self, written_after_fork, ManifestKind, ManifestRow, ManifestStatus};
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
    baseline: Option<HashMap<PathBuf, ManifestRow>>,
    need_line_counts: bool,
) -> Result<Vec<FileChange>> {
    let scan = CloneScan {
        workspace,
        filter,
        baseline_known: baseline.is_some(),
        baseline: baseline.unwrap_or_default(),
        need_line_counts,
    };

    let walk = scan.walk_view()?;
    let mut changes = walk.changes;
    changes.extend(scan.deletions(&walk.seen, &walk.pruned));
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

/// Read a `Ready` manifest keyed by decoded path (`Ok(None)` for a manifest that is not
/// ready). If it cannot be read, run [`manifest::change_count`] for its recovery side effects
/// (degrade, or schedule a rebuild) and return its verdict, so the scan goes without a baseline.
fn load_baseline(
    workspace: &Workspace,
    include_ignored: bool,
) -> Result<std::result::Result<Option<HashMap<PathBuf, ManifestRow>>, ChangeCount>> {
    if workspace.manifest_status != ManifestStatus::Ready {
        return Ok(Ok(None));
    }
    let rows = manifest::stream(&workspace.name).and_then(|rows| {
        rows.map(|row| {
            let row = row?;
            Ok((manifest::decode_path(&row.path)?, row))
        })
        .collect::<Result<HashMap<_, _>>>()
    });
    match rows {
        Ok(rows) => Ok(Ok(Some(rows))),
        Err(_) => Ok(Err(manifest::change_count(workspace, include_ignored)?)),
    }
}

struct CloneScan<'a> {
    workspace: &'a Workspace,
    /// `None` when ignored paths are included.
    filter: Option<ChangeFilter>,
    /// Manifest rows by path; empty when the manifest is unavailable.
    baseline: HashMap<PathBuf, ManifestRow>,
    /// Whether `baseline` was loaded. With a baseline, an entry without a row is new;
    /// without one, only a post-fork ctime marks it as changed.
    baseline_known: bool,
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
    /// Every path the walk visited, visible or not.
    seen: BTreeSet<PathBuf>,
    /// Invisible directories the walk did not descend into.
    pruned: BTreeSet<PathBuf>,
}

impl CloneScan<'_> {
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
            let row = self.baseline.get(relative);
            // Directories are reported only when they replace a non-directory entry.
            let reportable = !is_dir || row.is_some_and(|row| row.kind != ManifestKind::Dir);
            let change = (reportable && self.changed(path, metadata, row))
                .then(|| self.change(relative.to_path_buf(), Some(path), row));
            Ok(Step::Descend(Some(Seen::Visible(change))))
        })?;

        let mut seen = BTreeSet::new();
        let mut pruned = BTreeSet::new();
        let mut changes = Vec::new();
        for (path, item) in found {
            match item {
                Seen::Pruned => {
                    pruned.insert(path);
                }
                Seen::Hidden | Seen::Visible(None) => {
                    seen.insert(path);
                }
                Seen::Visible(Some(change)) => {
                    seen.insert(path);
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
            None => self.baseline_known || written,
        }
    }

    /// Manifest rows the walk did not see. A missing directory is reported once, not once
    /// per descendant; rows under pruned or invisible paths are not reported at all.
    fn deletions(&self, seen: &BTreeSet<PathBuf>, pruned: &BTreeSet<PathBuf>) -> Vec<FileChange> {
        let missing = |path: &Path, row: &ManifestRow| {
            !seen.contains(path)
                && !pruned.iter().any(|prefix| path.starts_with(prefix))
                && self.is_visible(path, row.kind == ManifestKind::Dir)
        };
        let missing_directories: BTreeSet<&Path> = self
            .baseline
            .iter()
            .filter(|(path, row)| row.kind == ManifestKind::Dir && missing(path, row))
            .map(|(path, _)| path.as_path())
            .collect();
        let under_missing_directory = |path: &Path| {
            path.ancestors()
                .skip(1)
                .any(|ancestor| missing_directories.contains(ancestor))
        };

        self.baseline
            .iter()
            .filter(|(path, row)| missing(path, row) && !under_missing_directory(path))
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
            (Some(_), None) if !self.baseline_known && fs::symlink_metadata(&base_path).is_ok() => {
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
