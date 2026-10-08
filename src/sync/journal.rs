//! Resumable operation journals.
//!
//! A merge or pull is recorded as a list of per-path steps before anything is applied. Each
//! step is an atomic rename, and `next` is persisted after every step, so an interrupted
//! operation can be resumed exactly where it stopped. A journal whose phase is `done` is
//! terminal and is swept on the next state load.
//!
//! A step for a file both sides changed installs a line-level merge staged in the journal's
//! staging directory (`<journal>.d/`) instead of the source entry, and only over the exact
//! target the merge was computed from.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::util::fs::{rename_exchange, rename_noreplace, same_contents, FlaggedRename};
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

/// Progress of a journaled operation, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OperationPhase {
    /// Recorded; no step applied yet.
    #[default]
    Planned,
    Applying,
    /// Every step applied; post-transfer work (install, commit, re-baseline) may remain.
    Applied,
    /// A pull is rebuilding the workspace's fork-time manifest.
    Rebaselining,
    /// Terminal; the journal is only waiting to be removed.
    Done,
}

/// Size and modification time of an entry, to notice it changed between planning and apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub size: u64,
    pub mtime_ns: i128,
}

impl Fingerprint {
    pub fn of(path: &Path) -> Option<Self> {
        let metadata = fs::symlink_metadata(path).ok()?;
        Some(Self {
            size: metadata.len(),
            mtime_ns: crate::util::fs::mtime_ns(&metadata),
        })
    }
}

/// A step installing a staged line-level merge rather than the source entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedStep {
    #[serde(with = "crate::changes::path_serde")]
    pub path: PathBuf,
    /// The target the merge was computed from; anything else there is a newer edit.
    pub expected: Fingerprint,
}

/// The durable record of one merge or pull.
#[derive(Debug, Serialize, Deserialize)]
pub struct OperationJournal {
    pub op: OperationKind,
    pub workspace: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub changes: Vec<FileChange>,
    /// Index of the first step in `changes` not yet applied.
    pub next: usize,
    #[serde(default)]
    pub phase: OperationPhase,
    pub fork_time_ns: u128,
    /// Steps whose content is a staged line-level merge.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged: Vec<MergedStep>,
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
            merged: Vec::new(),
        }
    }
}

/// Location of the journal for one operation on one workspace generation.
pub fn path(op: OperationKind, workspace: &str, fork_time_ns: u128) -> PathBuf {
    paths::journal_dir().join(format!("{}-{workspace}-{fork_time_ns}.json", op.as_str()))
}

/// Where the line-level merges of the operation journaled at `journal` are staged.
pub fn staging_dir(journal: &Path) -> PathBuf {
    journal.with_extension("d")
}

/// Durably write a journal: temp file, fsync, rename, fsync directory.
pub fn save(path: &Path, journal: &OperationJournal) -> Result {
    let bytes = serde_json::to_vec_pretty(journal)?;
    crate::util::write_atomic(path, &bytes)
}

/// Read and parse the journal at `path`.
pub fn load(path: &Path) -> Result<OperationJournal> {
    serde_json::from_slice(&fs::read(path).io_context(|| format!("read {}", path.display()))?)
        .map_err(|error| {
            ForkspaceError::StateCorrupted(format!("invalid operation journal: {error}"))
        })
}

/// Record a new operation and apply all of its steps.
pub fn start(
    op: OperationKind,
    workspace: &str,
    source: PathBuf,
    target: PathBuf,
    changes: Vec<FileChange>,
    fork_time_ns: u128,
    merged: Vec<MergedStep>,
) -> Result<PathBuf> {
    let path = path(op, workspace, fork_time_ns);
    let mut journal = OperationJournal::new(op, workspace, source, target, changes, fork_time_ns);
    journal.merged = merged;
    save(&path, &journal)?;
    apply_remaining(&path)?;
    Ok(path)
}

/// Apply every step from `next` onward, persisting progress after each one.
pub fn apply_remaining(path: &Path) -> Result<OperationJournal> {
    let mut journal = load(path)?;
    journal.phase = OperationPhase::Applying;
    save(path, &journal)?;
    let staging = staging_dir(path);
    while journal.next < journal.changes.len() {
        let change = &journal.changes[journal.next];
        match journal.merged.iter().find(|step| step.path == change.path) {
            Some(step) => apply_merged(&staging, &journal.target, step, journal.next, journal.op)?,
            None => apply_change(
                &journal.source,
                &journal.target,
                change,
                journal.next,
                journal.op.as_str(),
            )?,
        }
        journal.next += 1;
        save(path, &journal)?;
    }
    journal.phase = OperationPhase::Applied;
    save(path, &journal)?;
    Ok(journal)
}

/// Durably record that the operation at `path` has reached `phase`.
pub fn set_phase(path: &Path, phase: OperationPhase) -> Result {
    let mut journal = load(path)?;
    journal.phase = phase;
    save(path, &journal)
}

/// Delete a completed journal and its staged merges.
pub fn remove(path: &Path) -> Result {
    remove_existing(&staging_dir(path))?;
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
        if path.extension().and_then(|value| value.to_str()) == Some("d") {
            if !path.with_extension("json").exists() {
                changed |= fs::remove_dir_all(&path).is_ok();
            }
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

/// Apply one step: delete the target, or stage the source entry beside it and rename it into
/// place so the target is never observed half-written.
pub(crate) fn apply_change(
    source_root: &Path,
    target_root: &Path,
    change: &FileChange,
    index: usize,
    operation: &str,
) -> Result {
    let source = source_root.join(&change.path);
    let target = target_root.join(&change.path);
    if change.kind == ChangeKind::Deleted {
        return remove_existing(&target);
    }
    ensure_parent_dir(&target)?;
    let metadata =
        fs::symlink_metadata(&source).io_context(|| format!("inspect {}", source.display()))?;
    if metadata.is_dir() {
        return install_dir(&source, &metadata, &target);
    }
    let temporary = target.with_extension(format!("forkspace-{operation}-{index}.tmp"));
    stage_entry(&source, &metadata, &temporary)?;
    let existing = fs::symlink_metadata(&target).ok();
    if existing.as_ref().is_some_and(fs::Metadata::is_dir) {
        return replace_directory(&temporary, &target);
    }
    if change.kind == ChangeKind::Added {
        return install_new(&temporary, &target, change, operation);
    }
    fs::rename(&temporary, &target).io_context(|| format!("install {}", target.display()))
}

/// Install the staged merge of `step` over its target, provided the target is still the one
/// the merge was computed from. A target that already holds the merge is a step that ran
/// before a crash.
fn apply_merged(
    staging: &Path,
    target_root: &Path,
    step: &MergedStep,
    index: usize,
    op: OperationKind,
) -> Result {
    let staged = staging.join(&step.path);
    let target = target_root.join(&step.path);
    let unchanged = Fingerprint::of(&target) == Some(step.expected);
    if !unchanged && !same_entry(&staged, &target) {
        let paths = vec![step.path.display().to_string()];
        return Err(match op {
            OperationKind::Pull => ForkspaceError::PullConflict { paths },
            OperationKind::Merge => ForkspaceError::MergeConflict { paths },
        });
    }
    let metadata =
        fs::symlink_metadata(&staged).io_context(|| format!("inspect {}", staged.display()))?;
    let temporary = target.with_extension(format!("forkspace-{}-{index}.tmp", op.as_str()));
    let _ = remove_existing(&temporary);
    fs::copy(&staged, &temporary)
        .io_context(|| format!("copy {} -> {}", staged.display(), temporary.display()))?;
    fs::set_permissions(&temporary, metadata.permissions())
        .io_context(|| format!("set permissions on {}", temporary.display()))?;
    fs::rename(&temporary, &target).io_context(|| format!("install {}", target.display()))
}

/// Put the staged entry where a directory is. A plain rename cannot replace a directory with
/// a non-directory, so the two are swapped atomically and the old tree is deleted from the
/// staging name; the path is never missing. Without swap support the directory is removed
/// first, as before.
fn replace_directory(temporary: &Path, target: &Path) -> Result {
    match rename_exchange(temporary, target)
        .io_context(|| format!("swap {} into place", target.display()))?
    {
        FlaggedRename::Done => remove_existing(temporary),
        _ => {
            remove_existing(target)?;
            fs::rename(temporary, target).io_context(|| format!("install {}", target.display()))
        }
    }
}

/// Install an entry the plan expected to be new without clobbering one that appeared since
/// planning. An identical entry is a step that already ran before a crash, so resuming
/// succeeds; anything else is a conflict to resolve before `--resume`.
fn install_new(temporary: &Path, target: &Path, change: &FileChange, operation: &str) -> Result {
    let outcome = rename_noreplace(temporary, target)
        .io_context(|| format!("install {}", target.display()))?;
    match outcome {
        FlaggedRename::Done => Ok(()),
        FlaggedRename::Unsupported => {
            if fs::symlink_metadata(target).is_err() {
                return fs::rename(temporary, target)
                    .io_context(|| format!("install {}", target.display()));
            }
            appeared_since_planning(temporary, target, change, operation)
        }
        FlaggedRename::DestinationExists => {
            appeared_since_planning(temporary, target, change, operation)
        }
    }
}

fn appeared_since_planning(
    temporary: &Path,
    target: &Path,
    change: &FileChange,
    operation: &str,
) -> Result {
    let identical = same_entry(temporary, target);
    remove_existing(temporary)?;
    if identical {
        return Ok(());
    }
    let paths = vec![change.path.display().to_string()];
    Err(if operation == "pull" {
        ForkspaceError::PullConflict { paths }
    } else {
        ForkspaceError::MergeConflict { paths }
    })
}

/// Whether two entries are the same symlink or the same file contents.
fn same_entry(a: &Path, b: &Path) -> bool {
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(a_meta), Ok(b_meta)) if a_meta.file_type().is_symlink() => {
            b_meta.file_type().is_symlink() && fs::read_link(a).ok() == fs::read_link(b).ok()
        }
        (Ok(a_meta), Ok(b_meta)) if a_meta.is_file() && b_meta.is_file() => same_contents(a, b),
        _ => false,
    }
}

/// Directories are created in place (their entries are separate steps), then given the
/// source's permissions and modification time.
fn install_dir(source: &Path, metadata: &fs::Metadata, target: &Path) -> Result {
    if fs::symlink_metadata(target).is_ok_and(|existing| !existing.is_dir()) {
        remove_existing(target)?;
    }
    fs::create_dir_all(target).io_context(|| format!("create {}", target.display()))?;
    fs::set_permissions(target, metadata.permissions())
        .io_context(|| format!("set permissions on {}", target.display()))?;
    copy_modified_time(source, metadata, target, fs::OpenOptions::new().read(true))
}

/// Recreate a symlink, or copy a file with its permissions and modification time, at
/// `temporary`.
fn stage_entry(source: &Path, metadata: &fs::Metadata, temporary: &Path) -> Result {
    let _ = remove_existing(temporary);
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(source).io_context(|| format!("read {}", source.display()))?;
        return std::os::unix::fs::symlink(link, temporary)
            .io_context(|| format!("create {}", temporary.display()));
    }
    fs::copy(source, temporary)
        .io_context(|| format!("copy {} -> {}", source.display(), temporary.display()))?;
    copy_modified_time(
        source,
        metadata,
        temporary,
        fs::OpenOptions::new().write(true),
    )
}

fn copy_modified_time(
    source: &Path,
    metadata: &fs::Metadata,
    target: &Path,
    open: &fs::OpenOptions,
) -> Result {
    let modified = metadata
        .modified()
        .io_context(|| format!("read time from {}", source.display()))?;
    open.open(target)
        .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
        .io_context(|| format!("set time on {}", target.display()))
}

/// Remove whatever is at `path`, recursively for a directory; a missing path is not an error.
///
/// Metadata comes from `symlink_metadata`, so a symlink to a directory is removed as a link.
fn remove_existing(path: &Path) -> Result {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path).io_context(|| format!("remove {}", path.display()))
    } else {
        fs::remove_file(path).io_context(|| format!("remove {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::apply_change;
    use crate::changes::{ChangeKind, FileChange};
    use crate::error::ForkspaceError;

    fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    #[test]
    fn added_file_does_not_clobber_one_created_after_planning() {
        let (source, target) = dirs();
        fs::write(source.path().join("new.txt"), "from workspace\n").unwrap();
        fs::write(target.path().join("new.txt"), "created in base meanwhile\n").unwrap();
        let change = FileChange::new(PathBuf::from("new.txt"), ChangeKind::Added);

        let error = apply_change(source.path(), target.path(), &change, 0, "merge").unwrap_err();
        assert!(
            matches!(error, ForkspaceError::MergeConflict { .. }),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(target.path().join("new.txt")).unwrap(),
            "created in base meanwhile\n"
        );
        assert_eq!(
            fs::read_dir(target.path()).unwrap().count(),
            1,
            "staging file left behind"
        );
    }

    #[test]
    fn added_file_already_installed_by_an_interrupted_run_is_accepted() {
        let (source, target) = dirs();
        fs::write(source.path().join("new.txt"), "same\n").unwrap();
        fs::write(target.path().join("new.txt"), "same\n").unwrap();
        let change = FileChange::new(PathBuf::from("new.txt"), ChangeKind::Added);

        apply_change(source.path(), target.path(), &change, 0, "pull").unwrap();
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
    }

    #[test]
    fn file_replaces_a_directory_in_one_step() {
        let (source, target) = dirs();
        fs::write(source.path().join("thing"), "now a file\n").unwrap();
        fs::create_dir_all(target.path().join("thing/nested")).unwrap();
        fs::write(target.path().join("thing/nested/old"), "old\n").unwrap();
        let change = FileChange::new(PathBuf::from("thing"), ChangeKind::Modified);

        apply_change(source.path(), target.path(), &change, 3, "merge").unwrap();
        assert_eq!(
            fs::read_to_string(target.path().join("thing")).unwrap(),
            "now a file\n"
        );
        assert_eq!(
            fs::read_dir(target.path()).unwrap().count(),
            1,
            "old tree left behind"
        );
    }
}
