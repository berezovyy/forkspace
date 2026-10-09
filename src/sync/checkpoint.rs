//! Checkpoints: save a workspace's state and return to it later.
//!
//! A checkpoint stores only what changed since the fork (what `diff` shows, or with `all`
//! every changed path including ignored ones such as `node_modules`), in
//! `FORKSPACE_HOME/checkpoints/<workspace>/<id>/`: `record.json` lists each path and its change
//! kind, and `files/` holds the saved entries with their permissions and modification times.
//! Files are copied with `fs::copy`, which clones on APFS, Btrfs, and XFS.
//!
//! Restore compares the workspace now with the checkpoint: saved paths get their saved
//! version back, and paths changed only after the checkpoint go back to their fork-time version
//! (the frozen copy of a sealed overlay, or the fork-time copy of a snapshot workspace). Paths
//! with no known fork-time version are listed, never guessed. The writes run through a restore
//! journal, so an interrupted restore can be resumed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::{select_backend, BackendState};
use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::sync::ancestor;
use crate::sync::journal::{self, Fingerprint, MergedStep, OperationKind};
use crate::util::fs::ensure_private_dir;
use crate::util::{ensure_parent_dir, now_rfc3339, now_unix_ns, write_atomic, IoContext};
use crate::workspace::Workspace;

const RECORD: &str = "record.json";
const FILES: &str = "files";
/// Manual checkpoints kept per workspace.
const KEEP_MANUAL: usize = 20;
/// Automatic checkpoints (before `pull` and `restore`) kept per workspace.
const KEEP_AUTOMATIC: usize = 10;

/// One saved state of a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: u64,
    pub workspace: String,
    pub created: String,
    pub created_ns: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Taken by forkspace before a `pull` or `restore`.
    #[serde(default)]
    pub automatic: bool,
    /// Ignored paths (such as `node_modules`) were saved too.
    #[serde(default)]
    pub all: bool,
    /// Bytes of saved file content.
    pub bytes: u64,
    pub entries: Vec<Entry>,
}

/// A path that differed from the fork when the checkpoint was taken.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    #[serde(with = "crate::changes::path_serde")]
    pub path: PathBuf,
    pub kind: ChangeKind,
}

/// What a restore did, or could not do.
#[derive(Debug, Default)]
pub struct Restored {
    /// Paths written or removed.
    pub changed: Vec<PathBuf>,
    /// Paths changed after the checkpoint whose fork-time version is unknown; left as they are.
    pub unrestorable: Vec<PathBuf>,
    /// The id of the automatic checkpoint taken first, to undo the restore.
    pub safety: Option<u64>,
}

/// Save the current state of `workspace`.
pub fn create(
    workspace: &Workspace,
    label: Option<String>,
    all: bool,
    automatic: bool,
) -> Result<Checkpoint> {
    let root = workspace_dir(&workspace.name);
    ensure_private_dir(&paths::checkpoint_root())?;
    ensure_private_dir(&root)?;
    let id = list(&workspace.name)?
        .iter()
        .map(|checkpoint| checkpoint.id)
        .max()
        .unwrap_or(0)
        + 1;
    let staging = root.join(format!(".{id}.tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    let built = save(workspace, &staging, all).map(|(entries, bytes)| Checkpoint {
        id,
        workspace: workspace.name.clone(),
        created: now_rfc3339(),
        created_ns: now_unix_ns(),
        label,
        automatic,
        all,
        bytes,
        entries,
    });
    let checkpoint = match built.and_then(|checkpoint| {
        write_atomic(
            &staging.join(RECORD),
            &serde_json::to_vec_pretty(&checkpoint)?,
        )?;
        Ok(checkpoint)
    }) {
        Ok(checkpoint) => checkpoint,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    let destination = root.join(id.to_string());
    fs::rename(&staging, &destination)
        .io_context(|| format!("install checkpoint {}", destination.display()))?;
    prune(&workspace.name);
    Ok(checkpoint)
}

/// The checkpoints of `workspace`, oldest first.
pub fn list(workspace: &str) -> Result<Vec<Checkpoint>> {
    let Ok(entries) = fs::read_dir(workspace_dir(workspace)) else {
        return Ok(Vec::new());
    };
    let mut checkpoints: Vec<Checkpoint> = entries
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| serde_json::from_slice(&fs::read(entry.path().join(RECORD)).ok()?).ok())
        .collect();
    checkpoints.sort_by_key(|checkpoint| checkpoint.id);
    Ok(checkpoints)
}

/// Delete every checkpoint of `workspace` (it is being dropped).
pub fn remove_all(workspace: &str) -> Result {
    let path = workspace_dir(workspace);
    match fs::remove_dir_all(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).io_context(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Return `workspace` to checkpoint `id`, after taking an automatic checkpoint of its current
/// state so the restore itself can be undone.
pub fn restore(workspace: &Workspace, id: u64) -> Result<Restored> {
    let checkpoint = list(&workspace.name)?
        .into_iter()
        .find(|checkpoint| checkpoint.id == id)
        .ok_or_else(|| {
            ForkspaceError::InvalidArgument(format!(
                "workspace '{}' has no checkpoint {id} (run `forkspace checkpoints {}`)",
                workspace.name, workspace.name
            ))
        })?;
    let saved = workspace_dir(&workspace.name)
        .join(id.to_string())
        .join(FILES);
    let journal_path = journal::path(
        OperationKind::Restore,
        &workspace.name,
        workspace.fork_time_ns,
    );
    let staging = journal::staging_dir(&journal_path);
    let _ = fs::remove_dir_all(&staging);

    let view = &workspace.view_path;
    let mut steps = Vec::new();
    let mut changes = Vec::new();
    let mut unrestorable = Vec::new();
    let wanted: BTreeMap<&Path, ChangeKind> = checkpoint
        .entries
        .iter()
        .map(|entry| (entry.path.as_path(), entry.kind))
        .collect();

    for (path, kind) in &wanted {
        let current = view.join(path);
        if *kind == ChangeKind::Deleted {
            if fs::symlink_metadata(&current).is_ok() {
                changes.push(FileChange::new(path.to_path_buf(), ChangeKind::Deleted));
            }
            continue;
        }
        let source = saved.join(path);
        if fs::symlink_metadata(&source).is_ok_and(|metadata| metadata.is_dir()) {
            fs::create_dir_all(&current).io_context(|| format!("create {}", current.display()))?;
            continue;
        }
        if !same_entry(&source, &current) {
            stage(&source, &staging.join(path))?;
            steps.push(step(path, &current));
            changes.push(FileChange::new(path.to_path_buf(), ChangeKind::Modified));
        }
    }

    let backend = select_backend(Some(workspace.backend_kind))?;
    for change in backend.changes(workspace, checkpoint.all, false)? {
        if wanted.contains_key(change.path.as_path()) {
            continue;
        }
        let current = view.join(&change.path);
        match change.kind {
            ChangeKind::Added => {
                changes.push(FileChange::new(change.path.clone(), ChangeKind::Deleted));
            }
            ChangeKind::Modified | ChangeKind::Deleted => {
                match fork_time_version(workspace, &change.path) {
                    Some(original) => {
                        stage(&original, &staging.join(&change.path))?;
                        steps.push(step(&change.path, &current));
                        changes.push(FileChange::new(change.path.clone(), ChangeKind::Modified));
                    }
                    None => unrestorable.push(change.path.clone()),
                }
            }
        }
    }
    // Taken after staging, so retention cannot remove the checkpoint being restored first.
    let safety = create(
        workspace,
        Some(format!("before restore to {id}")),
        checkpoint.all,
        true,
    )?;
    // Writes first, in path order (parents before children), then deletions.
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes.sort_by_key(|change| change.kind == ChangeKind::Deleted);
    let changed: Vec<PathBuf> = changes.iter().map(|change| change.path.clone()).collect();
    if !changes.is_empty() {
        let path = journal::start(
            OperationKind::Restore,
            &workspace.name,
            view.clone(),
            view.clone(),
            changes,
            workspace.fork_time_ns,
            steps,
        )?;
        journal::set_phase(&path, journal::OperationPhase::Done)?;
        journal::remove(&path)?;
    } else {
        let _ = fs::remove_dir_all(&staging);
    }
    unrestorable.sort();
    Ok(Restored {
        changed,
        unrestorable,
        safety: Some(safety.id),
    })
}

/// Finish an interrupted restore of `workspace`. Returns whether there was one.
pub fn resume(workspace: &Workspace) -> Result<bool> {
    let Some(path) = journal::find_open(OperationKind::Restore, &workspace.name)? else {
        return Ok(false);
    };
    journal::apply_remaining(&path)?;
    journal::set_phase(&path, journal::OperationPhase::Done)?;
    journal::remove(&path)?;
    Ok(true)
}

fn workspace_dir(workspace: &str) -> PathBuf {
    paths::checkpoint_root().join(workspace)
}

/// Copy every changed path of `workspace` into `staging/files`.
fn save(workspace: &Workspace, staging: &Path, all: bool) -> Result<(Vec<Entry>, u64)> {
    let files = staging.join(FILES);
    fs::create_dir_all(&files).io_context(|| format!("create {}", files.display()))?;
    let backend = select_backend(Some(workspace.backend_kind))?;
    let mut entries = Vec::new();
    let mut bytes = 0;
    for change in backend.changes(workspace, all, false)? {
        if change.kind != ChangeKind::Deleted {
            let source = workspace.view_path.join(&change.path);
            bytes += stage(&source, &files.join(&change.path))?;
        }
        entries.push(Entry {
            path: change.path,
            kind: change.kind,
        });
    }
    Ok((entries, bytes))
}

/// Copy the file, symlink, or (empty) directory at `source` to `target` with its permissions
/// and modification time. Returns the bytes copied.
fn stage(source: &Path, target: &Path) -> Result<u64> {
    let metadata =
        fs::symlink_metadata(source).io_context(|| format!("inspect {}", source.display()))?;
    ensure_parent_dir(target)?;
    if metadata.is_dir() {
        fs::create_dir_all(target).io_context(|| format!("create {}", target.display()))?;
        return Ok(0);
    }
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(source).io_context(|| format!("read {}", source.display()))?;
        std::os::unix::fs::symlink(link, target)
            .io_context(|| format!("create {}", target.display()))?;
        return Ok(0);
    }
    let copied = fs::copy(source, target)
        .io_context(|| format!("copy {} to {}", source.display(), target.display()))?;
    let modified = metadata
        .modified()
        .io_context(|| format!("read time from {}", source.display()))?;
    fs::OpenOptions::new()
        .write(true)
        .open(target)
        .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
        .io_context(|| format!("set time on {}", target.display()))?;
    Ok(copied)
}

fn step(path: &Path, current: &Path) -> MergedStep {
    MergedStep {
        path: path.to_path_buf(),
        expected: Fingerprint::of(current),
    }
}

/// The version of `path` the workspace's changes are measured against, if known.
fn fork_time_version(workspace: &Workspace, path: &Path) -> Option<PathBuf> {
    match &workspace.backend_state {
        BackendState::Overlay(layers) if layers.lower.is_some() => {
            let original = workspace.ancestor_dir().join(path);
            fs::symlink_metadata(&original)
                .is_ok_and(|metadata| !metadata.is_dir())
                .then_some(original)
        }
        _ => ancestor::file(workspace, path),
    }
}

/// Whether `a` and `b` are the same symlink, or files with the same content, mode, and mtime.
fn same_entry(a: &Path, b: &Path) -> bool {
    let (Ok(left), Ok(right)) = (fs::symlink_metadata(a), fs::symlink_metadata(b)) else {
        return false;
    };
    if left.file_type().is_symlink() || right.file_type().is_symlink() {
        return left.file_type().is_symlink()
            && right.file_type().is_symlink()
            && fs::read_link(a).ok() == fs::read_link(b).ok();
    }
    left.is_file()
        && right.is_file()
        && left.permissions() == right.permissions()
        && left.modified().ok() == right.modified().ok()
        && crate::util::fs::same_contents(a, b)
}

/// Keep the newest [`KEEP_MANUAL`] manual and [`KEEP_AUTOMATIC`] automatic checkpoints.
fn prune(workspace: &str) {
    let Ok(checkpoints) = list(workspace) else {
        return;
    };
    for automatic in [false, true] {
        let limit = if automatic {
            KEEP_AUTOMATIC
        } else {
            KEEP_MANUAL
        };
        let kind: Vec<_> = checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.automatic == automatic)
            .collect();
        for checkpoint in kind.iter().take(kind.len().saturating_sub(limit)) {
            let _ = fs::remove_dir_all(workspace_dir(workspace).join(checkpoint.id.to_string()));
        }
    }
}
