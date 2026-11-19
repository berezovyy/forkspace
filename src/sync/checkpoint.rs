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

use walkdir::WalkDir;

use crate::backend::select_backend;
use crate::changes::filter::ChangeFilter;
use crate::changes::manifest::{self, ManifestKind, ManifestStatus};
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
    crate::disk::exclude_from_backups(&paths::checkpoint_root());
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
    crate::state::trash(
        &workspace_dir(workspace),
        &format!("checkpoints-{workspace}"),
    )
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
    let mut unrestorable = Vec::new();
    // The checkpoint's own entries first: they win over anything derived below.
    let mut wants: BTreeMap<PathBuf, Want> = checkpoint
        .entries
        .iter()
        .map(|entry| {
            let source = saved.join(&entry.path);
            let want = if entry.kind == ChangeKind::Deleted {
                Want::Delete
            } else if is_real_dir(&source) {
                Want::Dir
            } else {
                Want::Entry(source)
            };
            (entry.path.clone(), want)
        })
        .collect();

    // Where the checkpoint has a file, a symlink or nothing, it has nothing below: a change
    // there now goes with that entry, never back to the fork's version (CKPT-11).
    let leaves: Vec<PathBuf> = wants
        .iter()
        .filter(|(_, want)| !matches!(want, Want::Dir))
        .map(|(path, _)| path.clone())
        .collect();
    let under_leaf = |path: &Path| {
        path.ancestors()
            .skip(1)
            .any(|parent| leaves.iter().any(|leaf| leaf == parent))
    };
    let backend = select_backend(Some(workspace.backend_kind))?;
    for change in backend.changes(workspace, checkpoint.all, false)? {
        if wants.contains_key(&change.path) || under_leaf(&change.path) {
            continue;
        }
        match change.kind {
            ChangeKind::Added => {
                wants.insert(change.path, Want::Delete);
            }
            ChangeKind::Modified | ChangeKind::Deleted => {
                put_back_fork_version(
                    workspace,
                    &change.path,
                    checkpoint.all,
                    &mut wants,
                    &mut unrestorable,
                );
            }
        }
    }
    need_directories_above_writes(view, &mut wants);

    let mut steps = Vec::new();
    let mut changes = Vec::new();
    for (path, want) in &wants {
        let current = view_entry(view, path);
        let current = current.as_deref();
        match want {
            Want::Delete => {
                if current.is_some_and(|current| fs::symlink_metadata(current).is_ok()) {
                    changes.push(FileChange::new(path.clone(), ChangeKind::Deleted));
                }
                continue;
            }
            Want::Dir if current.is_some_and(is_real_dir) => continue,
            Want::Dir => {
                let staged = staging.join(path);
                fs::create_dir_all(&staged)
                    .io_context(|| format!("create {}", staged.display()))?;
            }
            Want::Entry(source) if current.is_some_and(|current| same_entry(source, current)) => {
                continue
            }
            Want::Entry(source) => {
                stage(source, &staging.join(path))?;
            }
        }
        steps.push(MergedStep {
            path: path.clone(),
            expected: current.and_then(Fingerprint::of),
        });
        changes.push(FileChange::new(path.clone(), ChangeKind::Modified));
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
            None,
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

/// `path` in the view, or `None` while a parent of it there is a file or symlink: the
/// restore puts a directory in that parent's place first, so nothing is at `path` for it, and
/// whatever a symlinked parent leads to is never looked at.
fn view_entry(view: &Path, path: &Path) -> Option<PathBuf> {
    let mut directory = view.to_path_buf();
    for component in path.parent().map(Path::components).into_iter().flatten() {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return None,
            Err(_) => break,
        }
    }
    Some(view.join(path))
}

/// What a restore puts at one path.
enum Want {
    /// A directory: created, or put where a file or symlink is now.
    Dir,
    /// This saved entry: the checkpoint's version, or the fork-time one.
    Entry(PathBuf),
    Delete,
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Plan to put `path` back as it was at fork: a file or symlink as it was, a directory with
/// everything the fork-time copy holds inside it (CKPT-09). What the checkpoint itself saves
/// at a path stays first. Paths whose fork-time version is unknown go to `unrestorable`.
fn put_back_fork_version(
    workspace: &Workspace,
    path: &Path,
    all: bool,
    wants: &mut BTreeMap<PathBuf, Want>,
    unrestorable: &mut Vec<PathBuf>,
) {
    let Some(original) = ancestor::entry(workspace, path) else {
        unrestorable.push(path.to_path_buf());
        return;
    };
    if !is_real_dir(&original) {
        wants.insert(path.to_path_buf(), Want::Entry(original));
        return;
    }
    if is_real_dir(&workspace.view_path.join(path)) {
        // Only the directory's own mode changed, which a restore does not put back.
        unrestorable.push(path.to_path_buf());
        return;
    }
    wants.insert(path.to_path_buf(), Want::Dir);
    for entry in WalkDir::new(&original).min_depth(1).follow_links(false) {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(inner) = entry.path().strip_prefix(&original) else {
            continue;
        };
        let want = if entry.file_type().is_dir() {
            Want::Dir
        } else {
            Want::Entry(entry.path().to_path_buf())
        };
        wants.entry(path.join(inner)).or_insert(want);
    }
    unrestorable.extend(missing_from_fork_copy(workspace, path, &original, all));
}

/// The entries the fork-time manifest has below directory `path` that its fork-time copy
/// (`original`) lacks, such as symlinks and large files where the copy keeps regular files
/// only: a restored directory comes back without them.
fn missing_from_fork_copy(
    workspace: &Workspace,
    path: &Path,
    original: &Path,
    all: bool,
) -> Vec<PathBuf> {
    if workspace.manifest_status != ManifestStatus::Ready {
        return Vec::new();
    }
    let Ok(manifest) = manifest::Manifest::open(&workspace.name) else {
        return Vec::new();
    };
    let filter = ChangeFilter::build(&workspace.view_path);
    manifest
        .under(&manifest::encode_path(path))
        .filter(|row| row.kind != ManifestKind::Dir && !row.dirty)
        .filter_map(|row| manifest::decode_path(row.path).ok())
        .filter(|relative| all || filter.is_visible(relative, false))
        .filter(|relative| {
            relative
                .strip_prefix(path)
                .is_ok_and(|inner| fs::symlink_metadata(original.join(inner)).is_err())
        })
        .collect()
}

/// A path the restore writes needs real directories above it: the checkpoint was taken with
/// them there. A file or symlink in the way now (CKPT-10), or an entry planned for deletion, is
/// replaced by a directory first.
fn need_directories_above_writes(view: &Path, wants: &mut BTreeMap<PathBuf, Want>) {
    let writes: Vec<PathBuf> = wants
        .iter()
        .filter(|(_, want)| !matches!(want, Want::Delete))
        .map(|(path, _)| path.clone())
        .collect();
    for path in writes {
        for parent in path.ancestors().skip(1) {
            if parent.as_os_str().is_empty() {
                break;
            }
            let blocked = !matches!(wants.get(parent), None | Some(Want::Dir))
                || fs::symlink_metadata(view.join(parent)).is_ok_and(|metadata| !metadata.is_dir());
            if blocked {
                wants.insert(parent.to_path_buf(), Want::Dir);
            }
        }
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
    for directory in expired(workspace) {
        let _ = fs::remove_dir_all(directory);
    }
}

/// Checkpoint directories of `workspace` beyond the retention limits.
pub fn expired(workspace: &str) -> Vec<PathBuf> {
    let Ok(checkpoints) = list(workspace) else {
        return Vec::new();
    };
    let mut expired = Vec::new();
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
            expired.push(workspace_dir(workspace).join(checkpoint.id.to_string()));
        }
    }
    expired
}
