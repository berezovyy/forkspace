//! Merge planning: which workspace changes to transfer to the base, and which conflict.
//!
//! A plan is pure inspection; [`crate::sync::merge`] applies it.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::backend::select_backend;
use crate::changes::filter::ChangeFilter;
use crate::changes::manifest::{self, ManifestRow, ManifestStatus};
use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::sync::{display_paths, is_transferable, render_plan};
use crate::util::fs::mtime_ns;
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
    /// Paths the base also changed since the fork.
    pub conflicts: Vec<PathBuf>,
    /// Paths whose fork-time state was not recorded, so a base-side edit cannot be ruled out.
    pub ancestor_unknown: Vec<PathBuf>,
    pub fork_time_ns: u128,
    /// Changes left out because they are ignored, runtime, or internal.
    pub skipped: usize,
}

/// Plan a merge of `workspace` into its base.
///
/// Conflicts are an error unless `overwrite_base` is set, in which case they are reported in
/// the plan and the workspace version wins.
pub fn build(workspace: &Workspace, overwrite_base: bool) -> Result<MergePlan> {
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
    let all = backend.changes(workspace, true, false)?;
    trace::time("plan", || {
        build_from_changes(workspace, source, all, overwrite_base)
    })
}

/// Plan a merge from an already-collected change list, reading entries from `source`.
pub fn build_from_changes(
    workspace: &Workspace,
    source: PathBuf,
    all: Vec<FileChange>,
    overwrite_base: bool,
) -> Result<MergePlan> {
    ensure_manifest_usable(workspace)?;
    let total = all.len();
    let changes: Vec<_> = ChangeFilter::build(&workspace.base)
        .filter_changes(all)
        .into_iter()
        .filter(|change| is_transferable(change.class))
        .collect();
    check_sources(&source, &changes)?;
    validate_space(&source, &workspace.base, &changes)?;

    let (conflicts, ancestor_unknown) = classify(workspace, &changes)?;
    if !conflicts.is_empty() && !overwrite_base {
        return Err(ForkspaceError::MergeConflict {
            paths: display_paths(&conflicts),
        });
    }
    Ok(MergePlan {
        workspace: workspace.name.clone(),
        source,
        target: workspace.base.clone(),
        skipped: total - changes.len(),
        changes,
        conflicts,
        ancestor_unknown,
        fork_time_ns: workspace.fork_time_ns,
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
            ("ancestor unknown", &plan.ancestor_unknown),
            ("conflicts", &plan.conflicts),
        ],
    )
}

/// Whether `path` still matches its fork-time manifest row, so a merge can overwrite it.
///
/// Stricter than [`ManifestRow::matches`]: a re-created symlink (same target, new mtime) also
/// counts as a base-side change.
pub(crate) fn matches_ancestor(path: &Path, row: &ManifestRow) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| row.matches(path, &metadata) && mtime_ns(&metadata) == row.mtime_ns)
}

fn ensure_manifest_usable(workspace: &Workspace) -> Result {
    if matches!(
        workspace.manifest_status,
        ManifestStatus::Building | ManifestStatus::Degraded
    ) {
        return Err(ForkspaceError::InvalidArgument(
            "merge requires a ready fork manifest; wait and retry, or preserve the changes and fork again if tracking is degraded"
                .to_string(),
        ));
    }
    Ok(())
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
/// when the workspace added a path that the base has since created too.
fn classify(workspace: &Workspace, changes: &[FileChange]) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let manifest_ready = workspace.manifest_status == ManifestStatus::Ready;
    let rows = if manifest_ready {
        ancestor_rows(&workspace.name)?
    } else {
        HashMap::new()
    };
    let mut conflicts = Vec::new();
    let mut ancestor_unknown = Vec::new();
    for change in changes {
        let current = workspace.base.join(&change.path);
        match rows.get(&change.path) {
            Some(row) if row.dirty => ancestor_unknown.push(change.path.clone()),
            Some(row) if !matches_ancestor(&current, row) => conflicts.push(change.path.clone()),
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

fn ancestor_rows(workspace: &str) -> Result<HashMap<PathBuf, ManifestRow>> {
    manifest::stream(workspace)?
        .map(|row| {
            let row = row?;
            Ok((manifest::decode_path(&row.path)?, row))
        })
        .collect()
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
