use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::select_backend;
use crate::diff::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::filter::{ChangeClass, ChangeFilter};
use crate::manifest::{self, ManifestKind, ManifestRow, ManifestStatus};
use crate::util::{ensure_parent_dir, forkspace_home, IoContext};
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

#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    op: String,
    workspace: String,
    source: PathBuf,
    target: PathBuf,
    changes: Vec<FileChange>,
    next: usize,
    fork_time_ns: u128,
}

fn timestamp_ns(metadata: &fs::Metadata) -> i128 {
    i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
}

fn matches_row(path: &Path, row: &ManifestRow) -> bool {
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
            Some(row) if !matches_row(&current, row) => conflicts.push(change.path.clone()),
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
    let target = CString::new(target.as_os_str().as_bytes()).map_err(|_| {
        ForkspaceError::InvalidArgument("merge target contains a NUL byte".to_string())
    })?;
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

fn journal_path(workspace: &str, fork_time_ns: u128) -> PathBuf {
    forkspace_home()
        .join("journal")
        .join(format!("merge-{workspace}-{fork_time_ns}.json"))
}

fn save_journal(path: &Path, journal: &Journal) -> Result {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory).io_context(|| format!("create {}", directory.display()))?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|error| ForkspaceError::Serialization(error.to_string()))?;
    fs::write(&temporary, bytes).io_context(|| format!("write {}", temporary.display()))?;
    fs::File::open(&temporary)
        .and_then(|file| file.sync_all())
        .io_context(|| format!("sync {}", temporary.display()))?;
    fs::rename(&temporary, path).io_context(|| format!("install {}", path.display()))?;
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .io_context(|| format!("sync {}", directory.display()))
}

fn remove_existing(path: &Path) -> Result {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).io_context(|| format!("remove {}", path.display()))
    } else {
        fs::remove_file(path).io_context(|| format!("remove {}", path.display()))
    }
}

fn apply(source_root: &Path, target_root: &Path, change: &FileChange, index: usize) -> Result {
    let source = source_root.join(&change.path);
    let target = target_root.join(&change.path);
    if matches!(change.kind, ChangeKind::Deleted) {
        return remove_existing(&target);
    }
    ensure_parent_dir(&target)?;
    let temporary = target.with_extension(format!("forkspace-merge-{index}.tmp"));
    let metadata =
        fs::symlink_metadata(&source).io_context(|| format!("inspect {}", source.display()))?;
    let _ = remove_existing(&temporary);
    if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(
            fs::read_link(&source).io_context(|| format!("read {}", source.display()))?,
            &temporary,
        )
        .io_context(|| format!("create {}", temporary.display()))?;
    } else if metadata.is_dir() {
        fs::create_dir_all(&temporary).io_context(|| format!("create {}", temporary.display()))?;
    } else {
        fs::copy(&source, &temporary)
            .io_context(|| format!("copy {} -> {}", source.display(), temporary.display()))?;
    }
    if fs::symlink_metadata(&target)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        remove_existing(&target)?;
    }
    fs::rename(&temporary, &target).io_context(|| format!("install {}", target.display()))
}

pub fn execute(plan: &MergePlan) -> Result<PathBuf> {
    let path = journal_path(&plan.workspace, plan.fork_time_ns);
    let journal = Journal {
        op: "merge".to_string(),
        workspace: plan.workspace.clone(),
        source: plan.source.clone(),
        target: plan.target.clone(),
        changes: plan.changes.clone(),
        next: 0,
        fork_time_ns: plan.fork_time_ns,
    };
    save_journal(&path, &journal)?;
    resume_path(&path)?;
    Ok(path)
}

pub fn execute_without_journal(plan: &MergePlan) -> Result {
    for (index, change) in plan.changes.iter().enumerate() {
        apply(&plan.source, &plan.target, change, index)?;
    }
    Ok(())
}

fn resume_path(path: &Path) -> Result {
    let mut journal: Journal =
        serde_json::from_slice(&fs::read(path).io_context(|| format!("read {}", path.display()))?)
            .map_err(|error| {
                ForkspaceError::StateCorrupted(format!("invalid merge journal: {error}"))
            })?;
    while journal.next < journal.changes.len() {
        apply(
            &journal.source,
            &journal.target,
            &journal.changes[journal.next],
            journal.next,
        )?;
        journal.next += 1;
        save_journal(path, &journal)?;
    }
    Ok(())
}

pub fn resume(workspace: &Workspace) -> Result<PathBuf> {
    let path = journal_path(&workspace.name, workspace.fork_time_ns);
    if !path.exists() {
        return Err(ForkspaceError::InvalidArgument(format!(
            "no interrupted merge journal for '{}'",
            workspace.name
        )));
    }
    resume_path(&path)?;
    Ok(path)
}

pub fn finish(path: &Path) -> Result {
    fs::remove_file(path).io_context(|| format!("remove {}", path.display()))
}
