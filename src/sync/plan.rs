//! Merge planning: which workspace changes to transfer to the base, and which conflict.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::backend::select_backend;
use crate::changes::filter::{ChangeClass, ChangeFilter};
use crate::changes::manifest::{self, ManifestKind, ManifestRow, ManifestStatus};
use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::sync::journal::{self, OperationKind};
use crate::util::IoContext;
use crate::workspace::Workspace;

#[derive(Debug, Clone)]
pub struct MergePlan {
    pub workspace: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub changes: Vec<FileChange>,
    pub conflicts: Vec<PathBuf>,
    pub ancestor_unknown: Vec<PathBuf>,
    pub fork_time_ns: u128,
    pub skipped: usize,
}

fn timestamp_ns(metadata: &fs::Metadata) -> i128 {
    i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
}

pub fn matches_ancestor(path: &Path, row: &ManifestRow) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    let kind = if metadata.file_type().is_symlink() {
        ManifestKind::Symlink
    } else if metadata.is_dir() {
        ManifestKind::Dir
    } else {
        ManifestKind::File
    };
    kind == row.kind
        && metadata.len() == row.size
        && timestamp_ns(&metadata) == row.mtime_ns
        && (row.mode == 0 || metadata.mode() & 0o7777 == row.mode)
        && (kind != ManifestKind::Symlink
            || fs::read_link(path)
                .ok()
                .is_some_and(|target| Some(manifest::encode_path(&target)) == row.target))
}

fn safe_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

pub fn build(workspace: &Workspace, overwrite_base: bool) -> Result<MergePlan> {
    if !workspace.base.is_dir() {
        return Err(ForkspaceError::InvalidArgument(format!(
            "base directory for '{}' is missing at {}; restore it before merging",
            workspace.name,
            workspace.base.display()
        )));
    }
    if matches!(
        workspace.manifest_status,
        ManifestStatus::Building | ManifestStatus::Degraded
    ) {
        return Err(ForkspaceError::InvalidArgument(
            "merge requires a ready fork manifest; wait and retry, or preserve the changes and fork again if tracking is degraded"
                .to_string(),
        ));
    }
    let source = if workspace.manifest_status == ManifestStatus::Absent {
        workspace
            .upper_path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| workspace.view_path.clone())
    } else {
        select_backend(Some(workspace.backend_kind))?.view_path(workspace)
    };
    let all = select_backend(Some(workspace.backend_kind))?.changes(workspace, true, false)?;
    build_from_changes(workspace, source, all, overwrite_base)
}

pub fn build_from_changes(
    workspace: &Workspace,
    source: PathBuf,
    all: Vec<FileChange>,
    overwrite_base: bool,
) -> Result<MergePlan> {
    if matches!(
        workspace.manifest_status,
        ManifestStatus::Building | ManifestStatus::Degraded
    ) {
        return Err(ForkspaceError::InvalidArgument(
            "merge requires a ready fork manifest; wait and retry, or preserve the changes and fork again if tracking is degraded"
                .to_string(),
        ));
    }
    let filter = ChangeFilter::build(&workspace.base);
    let total = all.len();
    let changes: Vec<_> = filter
        .filter_changes(all)
        .into_iter()
        .filter(|change| !matches!(change.class, ChangeClass::Runtime | ChangeClass::Internal))
        .collect();
    for change in &changes {
        if !safe_path(&change.path) {
            return Err(ForkspaceError::InvalidArgument(format!(
                "unsafe merge path '{}'",
                change.path.display()
            )));
        }
        if !matches!(change.kind, ChangeKind::Deleted) {
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
    }
    validate_space(&source, &workspace.base, &changes)?;
    let rows: HashMap<PathBuf, ManifestRow> = if workspace.manifest_status == ManifestStatus::Ready
    {
        manifest::stream(&workspace.name)?
            .map(|row| {
                let row = row?;
                Ok((manifest::decode_path(&row.path)?, row))
            })
            .collect::<Result<_>>()?
    } else {
        HashMap::new()
    };
    let mut conflicts = Vec::new();
    let mut ancestor_unknown = Vec::new();
    for change in &changes {
        let current = workspace.base.join(&change.path);
        match rows.get(&change.path) {
            Some(row) if row.dirty => ancestor_unknown.push(change.path.clone()),
            Some(row) if !matches_ancestor(&current, row) => conflicts.push(change.path.clone()),
            None if matches!(change.kind, ChangeKind::Added)
                && fs::symlink_metadata(&current).is_ok() =>
            {
                conflicts.push(change.path.clone());
            }
            None if workspace.manifest_status == ManifestStatus::Ready => {
                ancestor_unknown.push(change.path.clone());
            }
            _ => {}
        }
    }
    conflicts.sort();
    ancestor_unknown.sort();
    if !conflicts.is_empty() && !overwrite_base {
        return Err(ForkspaceError::MergeConflict {
            paths: conflicts
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        });
    }
    let skipped = total - changes.len();
    Ok(MergePlan {
        workspace: workspace.name.clone(),
        source,
        target: workspace.base.clone(),
        changes,
        conflicts,
        ancestor_unknown,
        fork_time_ns: workspace.fork_time_ns,
        skipped,
    })
}

fn validate_space(source: &Path, target: &Path, changes: &[FileChange]) -> Result {
    let required = changes
        .iter()
        .filter(|change| !matches!(change.kind, ChangeKind::Deleted))
        .filter_map(|change| fs::symlink_metadata(source.join(&change.path)).ok())
        .map(|metadata| metadata.len())
        .max()
        .unwrap_or(0);
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| ForkspaceError::InvalidArgument("merge target contains a NUL byte".to_string()))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(target.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(ForkspaceError::Io {
            context: "inspect free space for merge target".to_string(),
            source: std::io::Error::last_os_error(),
        });
    }
    let stats = unsafe { stats.assume_init() };
    let available = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
    if u128::from(required) > available {
        return Err(ForkspaceError::InvalidArgument(format!(
            "merge needs {required} free bytes for an atomic write; free space in the base volume and retry"
        )));
    }
    Ok(())
}

pub fn render(plan: &MergePlan) -> String {
    let mut lines = vec![format!(
        "merge plan for '{}': {} files",
        plan.workspace,
        plan.changes.len()
    )];
    for change in &plan.changes {
        let kind = match change.kind {
            ChangeKind::Added => "add",
            ChangeKind::Modified => "modify",
            ChangeKind::Deleted => "delete",
        };
        lines.push(format!("  {kind} {}", change.path.display()));
    }
    if !plan.ancestor_unknown.is_empty() {
        lines.push(format!(
            "ancestor unknown: {}",
            plan.ancestor_unknown
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !plan.conflicts.is_empty() {
        lines.push(format!(
            "conflicts: {}",
            plan.conflicts
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.join("\n")
}

pub fn execute(plan: &MergePlan) -> Result<PathBuf> {
    journal::start(
        OperationKind::Merge,
        &plan.workspace,
        plan.source.clone(),
        plan.target.clone(),
        plan.changes.clone(),
        plan.fork_time_ns,
    )
}

pub fn execute_without_journal(plan: &MergePlan) -> Result {
    for (index, change) in plan.changes.iter().enumerate() {
        journal::apply_change(&plan.source, &plan.target, change, index, "merge")?;
    }
    Ok(())
}

fn resume_path(path: &Path) -> Result {
    if journal::load(path)?.op != OperationKind::Merge {
        return Err(ForkspaceError::StateCorrupted(format!(
            "{} is not a merge journal",
            path.display()
        )));
    }
    journal::apply_remaining(path)?;
    Ok(())
}

pub fn resume(workspace: &Workspace) -> Result<PathBuf> {
    let path = journal::path(
        OperationKind::Merge,
        &workspace.name,
        workspace.fork_time_ns,
    );
    if !path.exists() {
        return Err(ForkspaceError::InvalidArgument(format!(
            "no interrupted merge journal for '{}'",
            workspace.name
        )));
    }
    resume_path(&path)?;
    Ok(path)
}
