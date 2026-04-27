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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    Merge,
    Pull,
}

impl OperationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Pull => "pull",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OperationPhase {
    #[default]
    Planned,
    Applying,
    Applied,
    Rebaselining,
    Done,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OperationJournal {
    pub op: OperationKind,
    pub workspace: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub changes: Vec<FileChange>,
    pub next: usize,
    #[serde(default)]
    pub phase: OperationPhase,
    pub fork_time_ns: u128,
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

pub fn journal_path(operation: OperationKind, workspace: &str, fork_time_ns: u128) -> PathBuf {
    forkspace_home().join("journal").join(format!(
        "{}-{workspace}-{fork_time_ns}.json",
        operation.as_str()
    ))
}

pub fn save_journal(path: &Path, journal: &OperationJournal) -> Result {
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

pub(crate) fn apply_change(
    source_root: &Path,
    target_root: &Path,
    change: &FileChange,
    index: usize,
    operation: &str,
) -> Result {
    let source = source_root.join(&change.path);
    let target = target_root.join(&change.path);
    if matches!(change.kind, ChangeKind::Deleted) {
        return remove_existing(&target);
    }
    ensure_parent_dir(&target)?;
    let temporary = target.with_extension(format!("forkspace-{operation}-{index}.tmp"));
    let metadata =
        fs::symlink_metadata(&source).io_context(|| format!("inspect {}", source.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        if fs::symlink_metadata(&target)
            .is_ok_and(|value| !value.is_dir() || value.file_type().is_symlink())
        {
            remove_existing(&target)?;
        }
        fs::create_dir_all(&target).io_context(|| format!("create {}", target.display()))?;
        fs::set_permissions(&target, metadata.permissions())
            .io_context(|| format!("set permissions on {}", target.display()))?;
        let modified = metadata
            .modified()
            .io_context(|| format!("read time from {}", source.display()))?;
        fs::OpenOptions::new()
            .read(true)
            .open(&target)
            .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
            .io_context(|| format!("set time on {}", target.display()))?;
        return Ok(());
    }
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
        let modified = metadata
            .modified()
            .io_context(|| format!("read time from {}", source.display()))?;
        fs::OpenOptions::new()
            .write(true)
            .open(&temporary)
            .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
            .io_context(|| format!("set time on {}", temporary.display()))?;
    }
    if fs::symlink_metadata(&target)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        remove_existing(&target)?;
    }
    fs::rename(&temporary, &target).io_context(|| format!("install {}", target.display()))
}

pub fn load_journal(path: &Path) -> Result<OperationJournal> {
    serde_json::from_slice(&fs::read(path).io_context(|| format!("read {}", path.display()))?)
        .map_err(|error| {
            ForkspaceError::StateCorrupted(format!("invalid operation journal: {error}"))
        })
}

pub fn apply_remaining(path: &Path) -> Result<OperationJournal> {
    let mut journal = load_journal(path)?;
    journal.phase = OperationPhase::Applying;
    save_journal(path, &journal)?;
    while journal.next < journal.changes.len() {
        apply_change(
            &journal.source,
            &journal.target,
            &journal.changes[journal.next],
            journal.next,
            journal.op.as_str(),
        )?;
        journal.next += 1;
        save_journal(path, &journal)?;
    }
    journal.phase = OperationPhase::Applied;
    save_journal(path, &journal)?;
    Ok(journal)
}

pub fn start_operation(
    operation: OperationKind,
    workspace: &str,
    source: PathBuf,
    target: PathBuf,
    changes: Vec<FileChange>,
    fork_time_ns: u128,
) -> Result<PathBuf> {
    let path = journal_path(operation, workspace, fork_time_ns);
    let journal = OperationJournal {
        op: operation,
        workspace: workspace.to_string(),
        source,
        target,
        changes,
        next: 0,
        phase: OperationPhase::Planned,
        fork_time_ns,
    };
    save_journal(&path, &journal)?;
    apply_remaining(&path)?;
    Ok(path)
}

pub fn set_operation_phase(path: &Path, phase: OperationPhase) -> Result {
    let mut journal = load_journal(path)?;
    journal.phase = phase;
    save_journal(path, &journal)
}

pub fn execute(plan: &MergePlan) -> Result<PathBuf> {
    start_operation(
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
        apply_change(&plan.source, &plan.target, change, index, "merge")?;
    }
    Ok(())
}

fn resume_path(path: &Path) -> Result {
    let journal = load_journal(path)?;
    if journal.op != OperationKind::Merge {
        return Err(ForkspaceError::StateCorrupted(format!(
            "{} is not a merge journal",
            path.display()
        )));
    }
    apply_remaining(path)?;
    Ok(())
}

pub fn resume(workspace: &Workspace) -> Result<PathBuf> {
    let path = journal_path(
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

pub fn finish(path: &Path) -> Result {
    fs::remove_file(path).io_context(|| format!("remove {}", path.display()))
}
