//! Merge planning: which workspace changes to transfer to the base, and which conflict.
//!
//! A plan is pure inspection; [`crate::sync::merge`] applies it.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;

use crate::backend::select_backend;
use crate::changes::content::files_differ;
use crate::changes::filter::ChangeFilter;
use crate::changes::manifest::{self, ManifestKind, ManifestRow, ManifestStatus};
use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::sync::ancestor;
use crate::sync::journal::{self, MergedStep, OperationKind, StepGuard};
use crate::sync::three_way::{has_conflict_markers, reconcile, Favor, Policy};
use crate::sync::{display_paths, is_transferable, order_respellings, render_plan};
use crate::util::fs::mtime_ns;
use crate::util::walk::Entry;
use crate::util::{trace, IoContext};
use crate::workspace::Workspace;

/// The changes a merge would transfer from a workspace into its base.
#[derive(Debug, Clone)]
pub struct MergePlan {
    pub workspace: String,
    /// Directory the changed entries are copied from.
    pub source: PathBuf,
    /// The base directory the changes land in.
    pub target: PathBuf,
    pub changes: Vec<FileChange>,
    /// Paths both sides changed that could not be combined: whole-file conflicts, and text
    /// files whose edits overlap.
    pub conflicts: Vec<PathBuf>,
    /// Paths both sides changed whose edits were combined line by line.
    pub auto_merged: Vec<PathBuf>,
    /// Lockfiles both sides changed: the base's version stays and the install step
    /// regenerates it from the merged manifest.
    pub kept_base: Vec<PathBuf>,
    /// Journal steps installing the staged line-level merges.
    pub merged_steps: Vec<MergedStep>,
    /// Paths whose fork-time state was not recorded, so a base-side edit cannot be ruled out.
    pub ancestor_unknown: Vec<PathBuf>,
    pub fork_time_ns: u128,
    /// Changes left out because they are ignored, runtime, or internal.
    pub skipped: usize,
    /// The base entries the changes replace, as planning found them.
    pub guard: StepGuard,
}

/// How a merge plan treats paths both sides changed.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanOptions {
    /// Overlapping hunks take the workspace's lines, and other conflicts the workspace's file.
    pub overwrite_base: bool,
    /// Report conflicts in the plan instead of failing, and stage nothing (dry runs).
    pub preview: bool,
    /// The install step runs after the merge, so a lockfile both sides changed can keep the
    /// base's version.
    pub install_runs: bool,
}

/// Plan a merge of `workspace` into its base. With `preview`, conflicts are reported in the
/// plan instead of failing it.
pub fn build(workspace: &Workspace, preview: bool) -> Result<MergePlan> {
    build_with(
        workspace,
        PlanOptions {
            preview,
            ..PlanOptions::default()
        },
    )
}

/// Plan a merge of `workspace` into its base.
pub fn build_with(workspace: &Workspace, options: PlanOptions) -> Result<MergePlan> {
    if !workspace.base.is_dir() {
        return Err(ForkspaceError::InvalidArgument(format!(
            "base directory for '{}' is missing at {}; restore it before merging",
            workspace.name,
            workspace.base.display()
        )));
    }
    ensure_manifest_usable(workspace)?;
    let backend = select_backend(Some(workspace.backend_kind))?;
    let source = if workspace.manifest_status == ManifestStatus::Absent {
        workspace
            .upper_path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| workspace.view_path.clone())
    } else {
        backend.view_path(workspace)
    };
    // A merge transfers only what `diff` shows. Snapshot backends walk the whole view, so
    // they skip ignored directories while walking; an overlay reads only its upper layer and
    // keeps the full list, so the plan can count what it leaves out.
    let include_ignored = !workspace.backend_kind.stacking_is_snapshot();
    let all = backend.changes(workspace, include_ignored, false)?;
    trace::time("plan", || plan_changes(workspace, source, all, options))
}

/// Plan a merge from an already-collected change list, reading entries from `source`. With
/// `overwrite_base`, conflicts are reported and the workspace wins.
pub fn build_from_changes(
    workspace: &Workspace,
    source: PathBuf,
    all: Vec<FileChange>,
    overwrite_base: bool,
) -> Result<MergePlan> {
    let options = PlanOptions {
        overwrite_base,
        ..PlanOptions::default()
    };
    plan_changes(workspace, source, all, options)
}

fn plan_changes(
    workspace: &Workspace,
    source: PathBuf,
    all: Vec<FileChange>,
    options: PlanOptions,
) -> Result<MergePlan> {
    ensure_manifest_usable(workspace)?;
    ensure_markers_resolved(workspace)?;
    let total = all.len();
    let mut changes: Vec<_> = ChangeFilter::build(&workspace.base)
        .filter_changes(all)
        .into_iter()
        .filter(|change| is_transferable(change.class))
        .collect();
    check_sources(&source, &changes)?;
    validate_space(&source, &workspace.base, &changes)?;
    let respelled = order_respellings(&workspace.base, &mut changes);

    // Before the base is read: an edit after this point is caught when its step runs.
    let mut guard = StepGuard::capture(
        &workspace.base,
        changes.iter().map(|change| change.path.as_path()),
    );
    let blocked = blocked_by_base_entries(&workspace.base, &changes);
    let (both_changed, ancestor_unknown) = classify(workspace, &source, &changes, &respelled)?;
    let staging = journal::staging_dir(&journal::path(
        OperationKind::Merge,
        &workspace.name,
        workspace.fork_time_ns,
    ));
    let _ = fs::remove_dir_all(&staging);
    let policy = Policy {
        favor: if options.overwrite_base && !options.preview {
            Favor::Workspace
        } else {
            Favor::Neither
        },
        allow_markers: false,
        install_runs: options.install_runs,
    };
    let reconciled = reconcile(
        workspace,
        &both_changed,
        &source,
        &workspace.base,
        &staging,
        policy,
    )?;
    let mut conflicts = [
        reconciled.overlapping.clone(),
        reconciled.whole_file.clone(),
        blocked.iter().map(|(path, _)| path.clone()).collect(),
    ]
    .concat();
    conflicts.sort();
    conflicts.dedup();
    if options.preview {
        let _ = fs::remove_dir_all(&staging);
    } else if !conflicts.is_empty() && !options.overwrite_base {
        let _ = fs::remove_dir_all(&staging);
        return Err(if reconciled.overlapping.is_empty() {
            ForkspaceError::MergeConflict {
                paths: display_paths(&conflicts),
            }
        } else {
            ForkspaceError::MergeOverlap {
                workspace: workspace.name.clone(),
                paths: display_paths(&conflicts),
            }
        });
    }
    changes.retain(|change| !reconciled.project_wins.contains(&change.path));
    let skipped = total - changes.len() - reconciled.project_wins.len();
    if options.overwrite_base && !options.preview {
        replace_blocking_entries(&workspace.base, &blocked, &mut changes, &mut guard);
    }
    Ok(MergePlan {
        workspace: workspace.name.clone(),
        source,
        target: workspace.base.clone(),
        skipped,
        changes,
        conflicts,
        auto_merged: reconciled.auto_merged,
        kept_base: reconciled.project_wins,
        merged_steps: if options.preview {
            Vec::new()
        } else {
            reconciled.steps
        },
        ancestor_unknown,
        fork_time_ns: workspace.fork_time_ns,
        guard,
    })
}

/// Refuse to merge while files still hold conflict markers that `pull` wrote.
fn ensure_markers_resolved(workspace: &Workspace) -> Result {
    let remaining: Vec<PathBuf> = workspace
        .unresolved_conflicts
        .iter()
        .filter(|path| has_conflict_markers(&workspace.view_path.join(path)))
        .cloned()
        .collect();
    if remaining.is_empty() {
        return Ok(());
    }
    Err(ForkspaceError::ConflictMarkers {
        workspace: workspace.name.clone(),
        paths: display_paths(&remaining),
    })
}

/// Human-readable plan, one line per change plus any ancestry and conflict notes.
pub fn render(plan: &MergePlan) -> String {
    render_plan(
        format!(
            "merge plan for '{}': {} files",
            plan.workspace,
            plan.changes.len()
        ),
        &plan.changes,
        |kind| match kind {
            ChangeKind::Added => "add",
            ChangeKind::Modified => "modify",
            ChangeKind::Deleted => "delete",
        },
        &[
            ("merged line by line", &plan.auto_merged),
            (
                "keeping the base lockfile (install regenerates it)",
                &plan.kept_base,
            ),
            ("ancestor unknown", &plan.ancestor_unknown),
            ("conflicts", &plan.conflicts),
        ],
    )
}

/// Whether `path` still matches its fork-time manifest row, so a merge can overwrite it.
///
/// Stricter than [`ManifestRow::matches`] for files: the mtime must match too. A symlink
/// matches on its target alone, which is all it holds: one re-created with the same target
/// (`git checkout`, `merge --undo`) has lost nothing (OVL-06). A directory matches on kind and
/// mode only: an APFS clone gives every directory a fresh mtime, and the entries inside are
/// compared on their own.
pub(crate) fn matches_ancestor(path: &Path, row: &ManifestRow) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        let entry = Entry::of(&metadata);
        if row.kind == ManifestKind::Dir {
            return metadata.is_dir() && (row.mode == 0 || entry.mode == row.mode);
        }
        row.matches(path, &entry)
            && (row.kind == ManifestKind::Symlink || mtime_ns(&metadata) == row.mtime_ns)
    })
}

/// Whether everything under the base directory `path` (`relative` in the workspace) is as it
/// was at fork, so deleting the directory loses no base-side change.
fn tree_matches_ancestor(
    workspace: &Workspace,
    path: &Path,
    relative: &Path,
    ancestor: &dyn Fn(&Path) -> Option<ManifestRow>,
) -> bool {
    WalkDir::new(path)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .all(|entry| {
            entry.is_ok_and(|entry| {
                let Ok(inner) = entry.path().strip_prefix(path) else {
                    return false;
                };
                let inner = relative.join(inner);
                let row = ancestor(&inner);
                if entry.file_type().is_dir() {
                    // Its entries are checked on their own; a directory created since the fork
                    // has no row.
                    return row.is_some_and(|row| row.kind == ManifestKind::Dir)
                        || !written_since_fork(workspace, entry.path());
                }
                base_holds_fork_version(workspace, &inner, entry.path(), row.as_ref())
            })
        })
}

/// Whether the base entry `path` (`relative` in the workspace) still holds the version the
/// workspace forked from, so overwriting or deleting it loses no base-side change.
///
/// Size, mtime and mode alone cannot tell: a rewrite can keep all three (`rsync -a`,
/// `touch -r`), and a dirty row was recorded after the workspace wrote the file. So an entry
/// counts as unchanged only if nothing wrote it since the fork (its ctime) and its row, when
/// clean, still matches; otherwise its bytes are compared with the fork-time copy. Without a
/// fork-time copy such an entry counts as changed.
pub(crate) fn base_holds_fork_version(
    workspace: &Workspace,
    relative: &Path,
    path: &Path,
    row: Option<&ManifestRow>,
) -> bool {
    if fs::symlink_metadata(path).is_err() {
        return false;
    }
    // A clean symlink row records the link's whole content, its target: no fork-time copy is
    // needed, which on Linux holds regular files only (OVL-06).
    if let Some(row) = row.filter(|row| row.kind == ManifestKind::Symlink && !row.dirty) {
        return matches_ancestor(path, row);
    }
    let untouched = !written_since_fork(workspace, path);
    let metadata_matches = row.is_none_or(|row| row.dirty || matches_ancestor(path, row));
    if untouched && metadata_matches {
        return true;
    }
    ancestor::entry(workspace, relative).is_some_and(|fork| !files_differ(&fork, path))
}

/// Whether every parent of `relative` under `root` is a real directory (or missing), so the
/// path names an entry of the tree itself rather than one behind a symlink.
fn parents_are_directories(root: &Path, relative: &Path) -> bool {
    let mut directory = root.to_path_buf();
    for component in relative.parent().into_iter().flat_map(Path::components) {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return false,
            Err(_) => return true,
        }
    }
    true
}

/// Whether `path` is a directory itself, not a symlink to one.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Whether `path` shows a write after the workspace's fork (its ctime).
fn written_since_fork(workspace: &Workspace, path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        manifest::written_after_fork(&Entry::of(&metadata), workspace.fork_time_ns)
    })
}

fn ensure_manifest_usable(workspace: &Workspace) -> Result {
    match workspace.manifest_status {
        ManifestStatus::Building => Err(ForkspaceError::InvalidArgument(format!(
            "merge requires a ready fork manifest; the manifest of '{}' is still being built, retry in a moment",
            workspace.name
        ))),
        ManifestStatus::Degraded => Err(ForkspaceError::InvalidArgument(format!(
            "merge requires a ready fork manifest; '{0}' lost its manifest after it was written to and has no fork-time copy to rebuild it from, so its deletions cannot be known: save its changes (`forkspace diff {0} --full`), then drop it and fork again",
            workspace.name
        ))),
        _ => Ok(()),
    }
}

/// Reject paths that could escape the base and entries a merge cannot reproduce.
fn check_sources(source: &Path, changes: &[FileChange]) -> Result {
    for change in changes {
        if !is_plain_relative(&change.path) {
            return Err(ForkspaceError::InvalidArgument(format!(
                "unsafe merge path '{}'",
                change.path.display()
            )));
        }
        if change.kind == ChangeKind::Deleted {
            continue;
        }
        let path = source.join(&change.path);
        let metadata = fs::symlink_metadata(&path)
            .io_context(|| format!("inspect merge source {}", path.display()))?;
        if !(metadata.is_file() || metadata.is_dir() || metadata.file_type().is_symlink()) {
            return Err(ForkspaceError::InvalidArgument(format!(
                "unsupported merge source kind at '{}'; use a file, directory, or symlink",
                change.path.display()
            )));
        }
    }
    Ok(())
}

fn is_plain_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Split changes into `(conflicts, ancestor_unknown)`, both sorted.
///
/// A change conflicts when the base entry no longer matches its fork-time manifest row, or
/// when the workspace added a path that the base has since created too. An addition under a
/// `respelled` path only renames an entry the plan deletes, which is checked on its own.
fn classify(
    workspace: &Workspace,
    source: &Path,
    changes: &[FileChange],
    respelled: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let manifest_ready = workspace.manifest_status == ManifestStatus::Ready;
    let rows = if manifest_ready {
        Some(manifest::Manifest::open(&workspace.name)?)
    } else {
        None
    };
    let ancestor = |relative: &Path| -> Option<ManifestRow> {
        let rows = rows.as_ref()?;
        rows.get(&manifest::encode_path(relative))
            .map(|row| row.to_row())
    };
    let mut conflicts = Vec::new();
    let mut ancestor_unknown = Vec::new();
    for change in changes {
        let current = workspace.base.join(&change.path);
        if change.kind == ChangeKind::Added
            && respelled
                .iter()
                .any(|spelling| change.path.starts_with(spelling))
        {
            continue;
        }
        if !parents_are_directories(&workspace.base, &change.path) {
            // A file or symlink in the base where a parent directory of the path would be:
            // the base has no entry at the path (a symlinked parent leads somewhere else,
            // MERGE-38). One the base had at fork it has deleted since; a parent the plan does
            // not replace is a conflict of its own (`blocked_by_base_entries`).
            let existed_at_fork = ancestor(&change.path).is_some_and(|row| {
                !row.dirty || ancestor::entry(workspace, &change.path).is_some()
            });
            if existed_at_fork {
                conflicts.push(change.path.clone());
            }
            continue;
        }
        match ancestor(&change.path) {
            // A clean directory row: kind and mode, then (for a deletion, or a replacement by
            // a file or symlink, which removes the tree just the same) every entry inside.
            Some(row) if row.kind == ManifestKind::Dir && !row.dirty => {
                let removes_tree =
                    change.kind == ChangeKind::Deleted || !is_real_dir(&source.join(&change.path));
                if !matches_ancestor(&current, &row)
                    || (removes_tree
                        && !tree_matches_ancestor(workspace, &current, &change.path, &ancestor))
                {
                    conflicts.push(change.path.clone());
                }
            }
            // The base no longer has the path: a conflict if it had it at fork (it deleted it
            // since), not if the workspace created it.
            Some(row) if fs::symlink_metadata(&current).is_err() => {
                let existed_at_fork = if row.dirty {
                    ancestor::entry(workspace, &change.path).is_some()
                } else {
                    true
                };
                if existed_at_fork {
                    conflicts.push(change.path.clone());
                }
            }
            // The base has a directory here, and the row is dirty or not a directory.
            Some(row) if is_real_dir(&current) => {
                // A clean row says what was here at fork; a dirty one was recorded after the
                // workspace changed the path, so the fork-time copy says, or failing that the
                // directory's ctime (a directory created since the fork has a new one).
                let directory_at_fork = if row.dirty {
                    match ancestor::entry(workspace, &change.path) {
                        Some(fork) => is_real_dir(&fork),
                        None => !written_since_fork(workspace, &current),
                    }
                } else {
                    row.kind == ManifestKind::Dir
                };
                if !directory_at_fork
                    || !tree_matches_ancestor(workspace, &current, &change.path, &ancestor)
                {
                    conflicts.push(change.path.clone());
                }
            }
            Some(row) => {
                if !base_holds_fork_version(workspace, &change.path, &current, Some(&row)) {
                    conflicts.push(change.path.clone());
                } else if row.dirty && ancestor::entry(workspace, &change.path).is_none() {
                    ancestor_unknown.push(change.path.clone());
                }
            }
            None if change.kind == ChangeKind::Added && fs::symlink_metadata(&current).is_ok() => {
                conflicts.push(change.path.clone());
            }
            None if manifest_ready => ancestor_unknown.push(change.path.clone()),
            _ => {}
        }
    }
    conflicts.sort();
    ancestor_unknown.sort();
    Ok((conflicts, ancestor_unknown))
}

/// The changes that write below a file or symlink the base has where the workspace has a
/// directory (MERGE-28), with that entry's path. Unless the plan changes that entry itself, a
/// step would have no directory to write into: a conflict.
fn blocked_by_base_entries(base: &Path, changes: &[FileChange]) -> Vec<(PathBuf, PathBuf)> {
    let planned: std::collections::BTreeSet<&Path> =
        changes.iter().map(|change| change.path.as_path()).collect();
    changes
        .iter()
        .filter(|change| change.kind != ChangeKind::Deleted)
        .filter_map(|change| {
            let mut parents: Vec<&Path> = change.path.ancestors().skip(1).collect();
            parents.reverse();
            let blocking = parents.into_iter().find(|parent| {
                !parent.as_os_str().is_empty()
                    && !planned.contains(parent)
                    && fs::symlink_metadata(base.join(parent))
                        .is_ok_and(|metadata| !metadata.is_dir())
            })?;
            Some((change.path.clone(), blocking.to_path_buf()))
        })
        .collect()
}

/// With `--overwrite-base`, plan to replace each blocking base entry with the workspace's
/// directory, right before the first change below it; undo keeps the entry.
fn replace_blocking_entries(
    base: &Path,
    blocked: &[(PathBuf, PathBuf)],
    changes: &mut Vec<FileChange>,
    guard: &mut StepGuard,
) {
    let mut entries: Vec<&PathBuf> = blocked.iter().map(|(_, entry)| entry).collect();
    entries.sort();
    entries.dedup();
    for entry in entries {
        let Some(first) = changes
            .iter()
            .position(|change| change.path.starts_with(entry))
        else {
            continue;
        };
        changes.insert(first, FileChange::new(entry.clone(), ChangeKind::Modified));
        guard.targets.push(journal::PlannedTarget {
            path: entry.clone(),
            state: journal::TargetState::of(&base.join(entry)),
        });
    }
}

/// Every non-deleted entry is staged next to its target before an atomic rename, so the base
/// volume must have room for the largest one.
fn validate_space(source: &Path, target: &Path, changes: &[FileChange]) -> Result {
    let required = changes
        .iter()
        .filter(|change| change.kind != ChangeKind::Deleted)
        .filter_map(|change| fs::symlink_metadata(source.join(&change.path)).ok())
        .map(|metadata| metadata.len())
        .max()
        .unwrap_or(0);
    if u128::from(required) > available_bytes(target)? {
        return Err(ForkspaceError::InvalidArgument(format!(
            "merge needs {required} free bytes for an atomic write; free space in the base volume and retry"
        )));
    }
    Ok(())
}

fn available_bytes(directory: &Path) -> Result<u128> {
    let directory = CString::new(directory.as_os_str().as_bytes()).map_err(|_| {
        ForkspaceError::InvalidArgument("merge target contains a NUL byte".to_string())
    })?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `directory` is NUL-terminated and `stats` is a valid out-pointer for statvfs.
    if unsafe { libc::statvfs(directory.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .io_context(|| "inspect free space for merge target".to_string());
    }
    // SAFETY: statvfs returned success, so it initialized `stats`.
    let stats = unsafe { stats.assume_init() };
    Ok(u128::from(stats.f_bavail) * u128::from(stats.f_frsize))
}
