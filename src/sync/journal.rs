//! Resumable operation journals.
//!
//! A merge or pull is recorded as a list of per-path steps before anything is applied. Each
//! step is an atomic rename, and `next` is persisted after every step, so an interrupted
//! operation can be resumed exactly where it stopped. A journal whose phase is `done` is
//! terminal and is swept on the next state load.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::util::{ensure_parent_dir, IoContext};

/// Upper bound on journal files inspected by one opportunistic sweep.
const SWEEP_LIMIT: usize = 128;

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

impl OperationJournal {
    fn new(
        op: OperationKind,
        workspace: &str,
        source: PathBuf,
        target: PathBuf,
        changes: Vec<FileChange>,
        fork_time_ns: u128,
    ) -> Self {
        Self {
            op,
            workspace: workspace.to_string(),
            source,
            target,
            changes,
            next: 0,
            phase: OperationPhase::Planned,
            fork_time_ns,
        }
    }
}

/// Location of the journal for one operation on one workspace generation.
pub fn path(op: OperationKind, workspace: &str, fork_time_ns: u128) -> PathBuf {
    paths::journal_dir().join(format!("{}-{workspace}-{fork_time_ns}.json", op.as_str()))
}

/// Durably write a journal: temp file, fsync, rename, fsync directory.
pub fn save(path: &Path, journal: &OperationJournal) -> Result {
    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|error| ForkspaceError::Serialization(error.to_string()))?;
    crate::util::write_atomic(path, &bytes)
}

pub fn load(path: &Path) -> Result<OperationJournal> {
    serde_json::from_slice(&fs::read(path).io_context(|| format!("read {}", path.display()))?)
        .map_err(|error| ForkspaceError::StateCorrupted(format!("invalid operation journal: {error}")))
}

/// Record a new operation and apply all of its steps.
pub fn start(
    op: OperationKind,
    workspace: &str,
    source: PathBuf,
    target: PathBuf,
    changes: Vec<FileChange>,
    fork_time_ns: u128,
) -> Result<PathBuf> {
    let path = path(op, workspace, fork_time_ns);
    let journal = OperationJournal::new(op, workspace, source, target, changes, fork_time_ns);
    save(&path, &journal)?;
    apply_remaining(&path)?;
    Ok(path)
}

/// Apply every step from `next` onward, persisting progress after each one.
pub fn apply_remaining(path: &Path) -> Result<OperationJournal> {
    let mut journal = load(path)?;
    journal.phase = OperationPhase::Applying;
    save(path, &journal)?;
    while journal.next < journal.changes.len() {
        apply_change(
            &journal.source,
            &journal.target,
            &journal.changes[journal.next],
            journal.next,
            journal.op.as_str(),
        )?;
        journal.next += 1;
        save(path, &journal)?;
    }
    journal.phase = OperationPhase::Applied;
    save(path, &journal)?;
    Ok(journal)
}

pub fn set_phase(path: &Path, phase: OperationPhase) -> Result {
    let mut journal = load(path)?;
    journal.phase = phase;
    save(path, &journal)
}

/// Delete a completed journal.
pub fn remove(path: &Path) -> Result {
    fs::remove_file(path).io_context(|| format!("remove {}", path.display()))
}

/// Find a non-terminal journal of kind `op` for `workspace`, if one exists.
pub fn find_open(op: OperationKind, workspace: &str) -> Result<Option<PathBuf>> {
    let directory = paths::journal_dir();
    let Ok(entries) = fs::read_dir(&directory) else {
        return Ok(None);
    };
    let prefix = format!("{}-{workspace}-", op.as_str());
    for entry in entries {
        let path = entry
            .io_context(|| format!("read journal directory {}", directory.display()))?
            .path();
        let matches = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".json"));
        if !matches {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).io_context(|| format!("read {}", path.display()))?,
        )
        .map_err(|error| {
            ForkspaceError::StateCorrupted(format!("invalid {} journal: {error}", op.as_str()))
        })?;
        if !is_terminal(&value) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Remove terminal journals and stray temp files. Returns whether anything was removed.
pub fn sweep_terminal() -> Result<bool> {
    let root = paths::journal_dir();
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(false);
    };
    let mut changed = false;
    for entry in entries.take(SWEEP_LIMIT) {
        let path = entry
            .io_context(|| format!("read journal directory {}", root.display()))?
            .path();
        if path.extension().and_then(|value| value.to_str()) == Some("tmp") {
            changed |= fs::remove_file(&path).is_ok();
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if is_terminal(&value) {
            changed |= fs::remove_file(&path).is_ok();
        }
    }
    Ok(changed)
}

/// Terminal check on raw JSON so journals written before `phase` existed still resolve:
/// such a merge journal is terminal once every step has been applied.
fn is_terminal(value: &serde_json::Value) -> bool {
    let phase = value.get("phase").and_then(|value| value.as_str());
    match value.get("op").and_then(|value| value.as_str()) {
        Some("merge") if phase.is_none() => {
            let next = value.get("next").and_then(|value| value.as_u64());
            let total = value
                .get("changes")
                .and_then(|value| value.as_array())
                .map(|values| values.len() as u64);
            next.zip(total).is_some_and(|(next, total)| next >= total)
        }
        Some("merge" | "pull") => phase == Some("done"),
        _ => false,
    }
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
