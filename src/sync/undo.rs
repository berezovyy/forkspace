//! Undo for `merge`: the project entries a merge replaces or deletes are kept, so
//! `forkspace merge --undo` can put the project back exactly as it was.
//!
//! Each merge gets `FORKSPACE_HOME/undo/<id>/` with `record.json` (one step per merged path)
//! and `kept/`, where the journal moves every entry it replaces or deletes. Moving is a
//! rename when the project and `FORKSPACE_HOME` share a filesystem, so it costs nothing that
//! merge would not have freed; otherwise the entry is copied. Undo restores a kept entry only
//! while the path still holds exactly what the merge installed (same size, mtime, and inode),
//! so later edits are never lost: such paths are reported and their kept versions stay.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::util::fs::{ensure_private_dir, exists_exact, mtime_ns};
use crate::util::{now_rfc3339, now_unix_ns, write_atomic, IoContext};

const RECORD: &str = "record.json";
const KEPT: &str = "kept";
/// Undo data is kept for at least this many merges...
const KEEP_MERGES: usize = 10;
/// ...and for at least this long.
const KEEP_NS: u128 = 14 * 24 * 3600 * 1_000_000_000;

/// What one merge changed in a project, and what is needed to reverse it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoRecord {
    pub workspace: String,
    pub project: PathBuf,
    pub merged_at: String,
    pub created_ns: u128,
    /// Every step was applied and the installed entries were recorded.
    #[serde(default)]
    pub complete: bool,
    pub steps: Vec<UndoStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoStep {
    #[serde(with = "crate::changes::path_serde")]
    pub path: PathBuf,
    pub kind: ChangeKind,
    /// The entry the merge left at `path`; `None` for a deletion.
    #[serde(default)]
    pub installed: Option<Identity>,
    /// The topmost directory the merge created to hold `path`, removed again on undo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_parent: Option<PathBuf>,
}

/// Enough of an entry's identity to tell whether it changed since the merge installed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub size: u64,
    pub mtime_ns: i128,
    pub ino: u64,
    pub dir: bool,
}

impl Identity {
    fn of(path: &Path) -> Option<Self> {
        let metadata = fs::symlink_metadata(path).ok()?;
        Some(Self {
            size: if metadata.is_dir() { 0 } else { metadata.len() },
            mtime_ns: if metadata.is_dir() {
                0
            } else {
                mtime_ns(&metadata)
            },
            ino: metadata.ino(),
            dir: metadata.is_dir(),
        })
    }
}

/// What `merge --undo` did.
#[derive(Debug, Default)]
pub struct UndoReport {
    pub workspace: String,
    pub project: PathBuf,
    /// Entries put back as they were before the merge.
    pub restored: usize,
    /// Entries the merge added, removed again.
    pub removed: usize,
    /// Paths changed after the merge; left alone, their pre-merge versions stay in the store.
    pub changed_since: Vec<PathBuf>,
    /// Paths a newer merge also changed; undo that merge first.
    pub newer_merge: Vec<PathBuf>,
}

/// Start recording a merge of `changes` from `workspace` into `project`. Returns the
/// record's directory, which the journal keeps replaced entries in.
pub fn begin(workspace: &str, project: &Path, changes: &[FileChange]) -> Result<PathBuf> {
    let root = paths::undo_root();
    ensure_private_dir(&root)?;
    crate::disk::exclude_from_backups(&root);
    let created_ns = now_unix_ns();
    let dir = root.join(format!("{created_ns:020}-{workspace}"));
    fs::create_dir_all(dir.join(KEPT)).io_context(|| format!("create {}", dir.display()))?;
    let steps = changes
        .iter()
        .map(|change| UndoStep {
            path: change.path.clone(),
            kind: change.kind,
            installed: None,
            created_parent: (change.kind != ChangeKind::Deleted)
                .then(|| topmost_missing_parent(project, &change.path))
                .flatten(),
        })
        .collect();
    let record = UndoRecord {
        workspace: workspace.to_string(),
        project: project.to_path_buf(),
        merged_at: now_rfc3339(),
        created_ns,
        complete: false,
        steps,
    };
    save(&dir, &record)?;
    Ok(dir)
}

/// Move `entry` (the pre-merge version of `relative`) into the store. The first version kept
/// is the pre-merge one, so a resumed step that runs again discards what it swapped out.
pub(crate) fn keep(dir: &Path, relative: &Path, entry: &Path) -> Result {
    if fs::symlink_metadata(entry).is_err() {
        return Ok(());
    }
    let kept = dir.join(KEPT).join(relative);
    if exists_exact(&dir.join(KEPT), relative) {
        return remove_entry(entry);
    }
    move_entry(entry, &kept)
}

/// Copy `entry` into the store before it is overwritten in place (no atomic swap here).
pub(crate) fn keep_copy(dir: &Path, relative: &Path, entry: &Path) -> Result {
    let kept = dir.join(KEPT).join(relative);
    if fs::symlink_metadata(entry).is_err() || exists_exact(&dir.join(KEPT), relative) {
        return Ok(());
    }
    copy_entry(entry, &kept)
}

/// Record what the merge installed and mark it undoable, then drop old undo data.
pub fn finish(dir: &Path) -> Result {
    let mut record = load(dir)?;
    for step in &mut record.steps {
        step.installed = match step.kind {
            ChangeKind::Deleted => None,
            ChangeKind::Added | ChangeKind::Modified => {
                Identity::of(&record.project.join(&step.path))
            }
        };
    }
    record.complete = true;
    save(dir, &record)?;
    prune();
    Ok(())
}

/// Undoable merges, newest first, with their directories.
pub fn list() -> Vec<(PathBuf, UndoRecord)> {
    let Ok(entries) = fs::read_dir(paths::undo_root()) else {
        return Vec::new();
    };
    let mut records: Vec<_> = entries
        .flatten()
        .filter_map(|entry| Some((entry.path(), load(&entry.path()).ok()?)))
        .filter(|(_, record)| record.complete)
        .collect();
    records.sort_by_key(|record| std::cmp::Reverse(record.1.created_ns));
    records
}

/// Undo the newest merge, or the newest merge from `workspace`.
pub fn undo(workspace: Option<&str>) -> Result<UndoReport> {
    let records = list();
    let Some(index) = records
        .iter()
        .position(|(_, record)| workspace.is_none_or(|name| record.workspace == name))
    else {
        return Err(ForkspaceError::InvalidArgument(match workspace {
            Some(name) => format!("no merge from '{name}' to undo"),
            None => "no merge to undo".to_string(),
        }));
    };
    let (dir, mut record) = records[index].clone();
    let newer: BTreeSet<PathBuf> = records[..index]
        .iter()
        .filter(|(_, newer)| newer.project == record.project)
        .flat_map(|(_, newer)| newer.steps.iter().map(|step| step.path.clone()))
        .collect();

    let mut report = UndoReport {
        workspace: record.workspace.clone(),
        project: record.project.clone(),
        ..UndoReport::default()
    };
    let mut remaining = Vec::new();
    for step in record.steps.iter().rev() {
        if newer.contains(&step.path) {
            report.newer_merge.push(step.path.clone());
            remaining.push(step.clone());
            continue;
        }
        match revert(&dir, &record.project, step)? {
            Reverted::Restored => report.restored += 1,
            Reverted::Removed => report.removed += 1,
            Reverted::Nothing => {}
            Reverted::ChangedSince => {
                report.changed_since.push(step.path.clone());
                remaining.push(step.clone());
            }
        }
    }
    report.changed_since.sort();
    report.newer_merge.sort();
    if remaining.is_empty() {
        fs::remove_dir_all(&dir).io_context(|| format!("remove {}", dir.display()))?;
    } else {
        remaining.reverse();
        record.steps = remaining;
        save(&dir, &record)?;
    }
    Ok(report)
}

enum Reverted {
    Restored,
    Removed,
    Nothing,
    ChangedSince,
}

/// Put one path back as it was before the merge.
fn revert(dir: &Path, project: &Path, step: &UndoStep) -> Result<Reverted> {
    let current = project.join(&step.path);
    let kept = dir.join(KEPT).join(&step.path);
    // Exactly this spelling: after a case-only rename the store keeps the old one.
    let has_kept = exists_exact(&dir.join(KEPT), &step.path);
    let present = Identity::of(&current);
    match step.installed {
        Some(installed) if installed.dir => {
            // Directories are containers: their entries are separate steps. One the merge
            // created goes again if it is empty now.
            if step.kind == ChangeKind::Added && !has_kept {
                let _ = fs::remove_dir(&current);
            }
            return Ok(Reverted::Nothing);
        }
        Some(installed) if present != Some(installed) => return Ok(Reverted::ChangedSince),
        Some(_) => remove_entry(&current)?,
        None if present.is_some() => return Ok(Reverted::ChangedSince),
        None => {}
    }
    let outcome = if has_kept {
        crate::util::ensure_parent_dir(&current)?;
        move_entry(&kept, &current)?;
        Reverted::Restored
    } else {
        Reverted::Removed
    };
    if let Some(created) = &step.created_parent {
        remove_empty_parents(project, &current, created);
    }
    Ok(outcome)
}

/// Remove the now-empty directories between `entry` and `created` (inclusive).
fn remove_empty_parents(project: &Path, entry: &Path, created: &Path) {
    let stop = project.join(created);
    let mut directory = entry.parent();
    while let Some(path) = directory {
        if !path.starts_with(&stop) || fs::remove_dir(path).is_err() {
            break;
        }
        directory = path.parent();
    }
}

/// The topmost ancestor of `relative` that does not exist in `project` yet.
fn topmost_missing_parent(project: &Path, relative: &Path) -> Option<PathBuf> {
    let mut missing = None;
    let mut parent = relative.parent();
    while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
        if exists_exact(project, path) {
            break;
        }
        missing = Some(path.to_path_buf());
        parent = path.parent();
    }
    missing
}

/// Keep undo data for the last [`KEEP_MERGES`] merges or [`KEEP_NS`], whichever is more.
fn prune() {
    for dir in expired() {
        let _ = fs::remove_dir_all(dir);
    }
}

/// Undo records beyond the retention limits.
pub fn expired() -> Vec<PathBuf> {
    let cutoff = now_unix_ns().saturating_sub(KEEP_NS);
    list()
        .into_iter()
        .skip(KEEP_MERGES)
        .filter(|(_, record)| record.created_ns < cutoff)
        .map(|(dir, _)| dir)
        .collect()
}

fn load(dir: &Path) -> Result<UndoRecord> {
    let path = dir.join(RECORD);
    serde_json::from_slice(&fs::read(&path).io_context(|| format!("read {}", path.display()))?)
        .map_err(|error| ForkspaceError::StateCorrupted(format!("invalid undo record: {error}")))
}

fn save(dir: &Path, record: &UndoRecord) -> Result {
    write_atomic(&dir.join(RECORD), &serde_json::to_vec_pretty(record)?)
}

/// Rename `from` to `to`, or copy and delete it across filesystems.
fn move_entry(from: &Path, to: &Path) -> Result {
    crate::util::ensure_parent_dir(to)?;
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            copy_entry(from, to)?;
            remove_entry(from)
        }
        Err(error) => {
            Err(error).io_context(|| format!("move {} to {}", from.display(), to.display()))
        }
    }
}

/// Copy a file, symlink, or directory tree, keeping permissions and modification times.
fn copy_entry(from: &Path, to: &Path) -> Result {
    crate::util::ensure_parent_dir(to)?;
    let status = std::process::Command::new("cp")
        .arg("-Rp")
        .arg(from)
        .arg(to)
        .status()
        .io_context(|| format!("copy {} to {}", from.display(), to.display()))?;
    if !status.success() {
        return Err(ForkspaceError::Io {
            context: format!("copy {} to {}", from.display(), to.display()),
            source: std::io::Error::other(format!("cp exited with {status}")),
        });
    }
    Ok(())
}

fn remove_entry(path: &Path) -> Result {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path).io_context(|| format!("remove {}", path.display()))
    } else {
        fs::remove_file(path).io_context(|| format!("remove {}", path.display()))
    }
}
