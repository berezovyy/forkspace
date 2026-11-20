//! Resumable operation journals.
//!
//! A merge or pull is recorded as a list of per-path steps before anything is applied. Each
//! step is an atomic rename that is safe to apply again, and `next` is persisted at least
//! every [`SAVE_INTERVAL`], so an interrupted operation resumes at most that far back from
//! where it stopped. A journal whose phase is `done` is
//! terminal and is swept on the next state load.
//!
//! A step for a file both sides changed installs a line-level merge staged in the journal's
//! staging directory (`<journal>.d/`) instead of the source entry, and only over the exact
//! target the merge was computed from.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::sync::undo::{self, Identity};
use crate::util::fs::{
    ctime_ns, exchanges_directories, rename_exchange, rename_noreplace, same_contents,
    FlaggedRename,
};
use crate::util::{ensure_parent_dir, IoContext};

/// Upper bound on journal files inspected by one opportunistic sweep.
const SWEEP_LIMIT: usize = 128;

/// How much applying may go unrecorded. Each save is a full flush (~45 ms on APFS), so saving
/// after every step made large merges and pulls take minutes.
const SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    Merge,
    Pull,
    /// `forkspace restore`: a workspace returned to one of its checkpoints.
    Restore,
}

impl OperationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Pull => "pull",
            Self::Restore => "restore",
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

/// What a step's target held when the operation was planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetState {
    Absent,
    /// A directory; its entries are compared on their own.
    Dir,
    /// A file or symlink, by size and modification time.
    Entry {
        size: u64,
        mtime_ns: i128,
    },
}

impl TargetState {
    pub fn of(path: &Path) -> Self {
        match fs::symlink_metadata(path) {
            Err(_) => Self::Absent,
            Ok(metadata) if metadata.is_dir() => Self::Dir,
            Ok(metadata) => Self::Entry {
                size: metadata.len(),
                mtime_ns: crate::util::fs::mtime_ns(&metadata),
            },
        }
    }
}

/// One target as the plan saw it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTarget {
    #[serde(with = "crate::changes::path_serde")]
    pub path: PathBuf,
    pub state: TargetState,
}

/// What the targets of an operation's steps held when it was planned. A step whose target
/// changed since (a base edit during a merge, a write by the agent during a pull, an edit made
/// while the operation was interrupted) leaves the newer entry alone and records its path as
/// held, instead of replacing or deleting it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepGuard {
    /// When the targets were captured: an entry with a later ctime was written since.
    pub planned_ns: u128,
    pub targets: Vec<PlannedTarget>,
    /// For a pull: when the plan started reading the base. A base entry with a later ctime
    /// may have changed after the plan read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ns: Option<u128>,
}

impl StepGuard {
    /// Capture the targets under `root` of `paths`. Call this before anything reads the
    /// contents the plan is built from, so a write after that read is always caught.
    pub fn capture<'a>(root: &Path, paths: impl IntoIterator<Item = &'a Path>) -> Self {
        let planned_ns = crate::util::fs::ctime_clock_ns();
        let targets = paths
            .into_iter()
            .map(|path| PlannedTarget {
                path: path.to_path_buf(),
                state: TargetState::of(&root.join(path)),
            })
            .collect();
        Self {
            planned_ns,
            targets,
            base_ns: None,
        }
    }

    /// Whether the plan saw an entry at `relative`.
    fn planned_entry(&self, relative: &Path) -> bool {
        self.targets
            .iter()
            .any(|planned| planned.path == relative && planned.state != TargetState::Absent)
    }

    /// Whether `target` (`relative` in the tree) is no longer what the plan saw: another
    /// kind, size or mtime, or a file or symlink written after planning (a rewrite that keeps
    /// size and mtime, as `rsync -a` and `touch -r` do, still moves its ctime).
    fn changed_since_planning(&self, relative: &Path, target: &Path) -> bool {
        let now = TargetState::of(target);
        let planned = self
            .targets
            .iter()
            .find(|planned| planned.path == relative)
            .map(|planned| planned.state);
        if planned.is_some_and(|planned| planned != now) {
            return true;
        }
        matches!(now, TargetState::Entry { .. }) && written_after(target, self.planned_ns)
    }
}

/// Whether `path` has a ctime after `instant_ns`.
fn written_after(path: &Path, instant_ns: u128) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| ctime_ns(&metadata) > i128::try_from(instant_ns).unwrap_or(i128::MAX))
}

/// What one step did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Done,
    /// The target changed since planning; these entries were left as they are.
    Held(Vec<PathBuf>),
}

/// A step installing a staged entry (a line-level merge, or a checkpoint's saved version)
/// rather than the source entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedStep {
    #[serde(with = "crate::changes::path_serde")]
    pub path: PathBuf,
    /// The target the entry was staged against (`None`: no entry there); anything else there
    /// is a newer edit.
    pub expected: Option<Fingerprint>,
}

/// A base change as a pull planned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseChange {
    #[serde(with = "crate::changes::path_serde")]
    pub path: PathBuf,
    pub kind: ChangeKind,
}

/// What a pull did about one base change it planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// No step: both sides agreed, the workspace keeps the path, or a step not applied yet.
    Unstepped,
    /// Left for the next pull: it conflicts with the workspace, found by a catch-up round.
    Deferred,
    /// Left alone: the workspace changed it while the pull ran.
    Held,
    /// A line-level merge of both sides was installed.
    Merged,
    /// The view's entry was deleted (or replaced by the deletion of a parent).
    Deleted,
    /// The base's entry was installed; the view's entry was this one right after.
    Installed(Identity),
}

/// A base change an earlier round of the same pull planned, with what came of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pulled {
    #[serde(with = "crate::changes::path_serde")]
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub outcome: Outcome,
}

/// What a pull journal records beyond its steps, so a catch-up round knows what the pull
/// already planned and did (see `sync::pull::history`).
#[derive(Debug, Clone, Default)]
pub struct PullRecord {
    /// Every base change this round planned, including those without a step.
    pub base_delta: Vec<BaseChange>,
    /// What earlier rounds planned and did.
    pub earlier: Vec<Pulled>,
    /// Base changes this round leaves for the next pull.
    pub deferred: Vec<PathBuf>,
    /// Paths earlier rounds held.
    pub held: Vec<PathBuf>,
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
    /// For a merge: the undo record that keeps every project entry this operation replaces
    /// or deletes (see [`crate::sync::undo`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<PathBuf>,
    /// The plan-time state of each target. Journals written before it existed, and restore
    /// journals, check nothing but the containment of each path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<StepGuard>,
    /// Entries left alone because they changed after planning (see [`StepGuard`]).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "crate::changes::path_serde::vec"
    )]
    pub held: Vec<PathBuf>,
    /// What each step installed, by step index, so undo (or a pull catching up with a base
    /// that moved) can tell it from a later edit. `None` for deletions, held steps and steps
    /// not applied yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub installed: Vec<Option<Identity>>,
    /// For a pull: every base change this round planned (see [`PullRecord`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_delta: Vec<BaseChange>,
    /// For a pull: what its earlier rounds planned and did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<Pulled>,
    /// For a pull: base changes this round leaves for the next pull.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "crate::changes::path_serde::vec"
    )]
    pub deferred: Vec<PathBuf>,
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
            undo: None,
            guard: None,
            held: Vec::new(),
            installed: Vec::new(),
            base_delta: Vec::new(),
            earlier: Vec::new(),
            deferred: Vec::new(),
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

/// Record a new operation and apply all of its steps. With a `guard`, a step leaves a target
/// that changed since planning alone (see [`StepGuard`]); [`held`] lists those afterwards.
#[allow(clippy::too_many_arguments)]
pub fn start(
    op: OperationKind,
    workspace: &str,
    source: PathBuf,
    target: PathBuf,
    changes: Vec<FileChange>,
    fork_time_ns: u128,
    merged: Vec<MergedStep>,
    guard: Option<StepGuard>,
) -> Result<PathBuf> {
    begin(
        op,
        workspace,
        source,
        target,
        changes,
        fork_time_ns,
        merged,
        guard,
        PullRecord::default(),
    )
}

/// [`start`] for one round of a pull, with what the pull planned and did so far.
#[allow(clippy::too_many_arguments)]
pub fn start_pull(
    workspace: &str,
    source: PathBuf,
    target: PathBuf,
    changes: Vec<FileChange>,
    fork_time_ns: u128,
    merged: Vec<MergedStep>,
    guard: StepGuard,
    record: PullRecord,
) -> Result<PathBuf> {
    begin(
        OperationKind::Pull,
        workspace,
        source,
        target,
        changes,
        fork_time_ns,
        merged,
        Some(guard),
        record,
    )
}

#[allow(clippy::too_many_arguments)]
fn begin(
    op: OperationKind,
    workspace: &str,
    source: PathBuf,
    target: PathBuf,
    changes: Vec<FileChange>,
    fork_time_ns: u128,
    merged: Vec<MergedStep>,
    guard: Option<StepGuard>,
    record: PullRecord,
) -> Result<PathBuf> {
    let path = path(op, workspace, fork_time_ns);
    let changes = if op == OperationKind::Restore {
        changes
    } else {
        without_covered(&source, changes, &merged)
    };
    let mut journal = OperationJournal::new(op, workspace, source, target, changes, fork_time_ns);
    journal.merged = merged;
    journal.guard = guard;
    journal.base_delta = record.base_delta;
    journal.earlier = record.earlier;
    journal.deferred = record.deferred;
    journal.held = record.held;
    // Resuming treats a planned and an applying journal alike, so the first save already says
    // `Applying` and [`apply_remaining`] does not write it a second time.
    journal.phase = OperationPhase::Applying;
    if op == OperationKind::Merge {
        journal.undo = Some(undo::begin(workspace, &journal.target, &journal.changes)?);
    }
    save(&path, &journal)?;
    apply_remaining(&path)?;
    Ok(path)
}

/// Apply every step from `next` onward, persisting progress at least every [`SAVE_INTERVAL`].
pub fn apply_remaining(path: &Path) -> Result<OperationJournal> {
    let mut journal = load(path)?;
    if journal.phase != OperationPhase::Applying {
        journal.phase = OperationPhase::Applying;
        save(path, &journal)?;
    }
    let mut saved = std::time::Instant::now();
    let mut unsaved = false;
    let staging = staging_dir(path);
    let undo = journal.undo.clone();
    if journal.installed.len() < journal.changes.len() {
        journal.installed.resize(journal.changes.len(), None);
    }
    while journal.next < journal.changes.len() {
        let change = &journal.changes[journal.next];
        // A step that already ran (before a crash, since the last save) installed this; an
        // entry there now that is not it was written since.
        let step = if let Some(installed) = journal.installed[journal.next] {
            if Identity::of(&journal.target.join(&change.path)) == Some(installed) {
                Step::Done
            } else {
                Step::Held(vec![change.path.clone()])
            }
        } else {
            match journal.merged.iter().find(|step| step.path == change.path) {
                Some(step) => apply_merged(
                    &staging,
                    &journal.target,
                    step,
                    journal.next,
                    journal.op,
                    undo.as_deref(),
                )?,
                None => apply_change(
                    &journal.source,
                    &journal.target,
                    change,
                    journal.next,
                    journal.op.as_str(),
                    undo.as_deref(),
                    journal.guard.as_ref(),
                )?,
            }
        };
        match step {
            Step::Done if change.kind != ChangeKind::Deleted => {
                journal.installed[journal.next] = Identity::of(&journal.target.join(&change.path));
            }
            Step::Done => {}
            Step::Held(paths) => {
                for path in paths {
                    if !journal.held.contains(&path) {
                        journal.held.push(path);
                    }
                }
            }
        }
        journal.next += 1;
        unsaved = true;
        if saved.elapsed() >= SAVE_INTERVAL {
            save(path, &journal)?;
            saved = std::time::Instant::now();
            unsaved = false;
        }
    }
    journal.phase = OperationPhase::Applied;
    // What the last steps held and installed is read after this returns (to report the held
    // paths, to make the merge undoable, to catch a pull up with its base). A crash before
    // this save only makes a resume repeat those steps, which converges.
    if unsaved {
        save(path, &journal)?;
    }
    Ok(journal)
}

/// Durably record that the operation at `path` has reached `phase`.
pub fn set_phase(path: &Path, phase: OperationPhase) -> Result {
    let mut journal = load(path)?;
    journal.phase = phase;
    save(path, &journal)
}

/// Make the merge journaled at `path` undoable, once everything it changes is in place.
pub fn finish_undo(path: &Path) -> Result {
    let journal = load(path)?;
    match &journal.undo {
        Some(dir) => undo::finish(dir, &journal.installed, &journal.held),
        None => Ok(()),
    }
}

/// The entries the operation journaled at `path` left alone because they changed after it
/// was planned.
pub fn held(path: &Path) -> Result<Vec<PathBuf>> {
    let mut held = load(path)?.held;
    held.sort();
    Ok(held)
}

/// Drop every step that a step on one of its ancestors already covers: the deletion of a
/// directory, or its replacement by a file or symlink (a staged merge is a file) removes the
/// whole tree in one rename, kept whole by undo. Applied one by one after that step, the
/// entries' own steps would act through whatever the step installed there, such as a symlink
/// to a directory outside the tree (FS-01).
fn without_covered(
    source: &Path,
    changes: Vec<FileChange>,
    merged: &[MergedStep],
) -> Vec<FileChange> {
    let covering: std::collections::BTreeSet<PathBuf> = changes
        .iter()
        .filter(|change| {
            change.kind == ChangeKind::Deleted
                || merged.iter().any(|step| step.path == change.path)
                || fs::symlink_metadata(source.join(&change.path))
                    .is_ok_and(|metadata| !metadata.is_dir())
        })
        .map(|change| change.path.clone())
        .collect();
    if covering.is_empty() {
        return changes;
    }
    changes
        .into_iter()
        .filter(|change| {
            !change
                .path
                .ancestors()
                .skip(1)
                .any(|ancestor| covering.contains(ancestor))
        })
        .collect()
}

/// The first ancestor of `relative` under `root` that is not a real directory (a symlink,
/// or a file), if any. Acting on `root/relative` would then reach outside the tree, or into
/// nothing. Missing ancestors are fine: they are created as directories.
fn escaping_parent(root: &Path, relative: &Path) -> Option<PathBuf> {
    let mut directory = root.to_path_buf();
    let parent = relative.parent()?;
    for component in parent.components() {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Some(directory),
            Err(_) => return None,
        }
    }
    None
}

/// A step that cannot run because a parent of its path is no longer a directory of the tree:
/// held when the operation is guarded, else an error.
fn refuse_escape(relative: &Path, parent: &Path, guarded: bool) -> Result<Step> {
    if guarded {
        return Ok(Step::Held(vec![relative.to_path_buf()]));
    }
    Err(ForkspaceError::InvalidArgument(format!(
        "refusing to write {}: {} is no longer a directory",
        relative.display(),
        parent.display()
    )))
}

/// Whether `target` already holds what the step installs (or, for a deletion, is gone): a
/// step that ran before a crash.
fn already_applied(source: &Path, target: &Path, kind: ChangeKind) -> bool {
    if kind == ChangeKind::Deleted {
        return fs::symlink_metadata(target).is_err();
    }
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.is_dir() => {
            fs::symlink_metadata(target).is_ok_and(|metadata| metadata.is_dir())
        }
        Ok(_) => same_entry(source, target),
        Err(_) => false,
    }
}

/// The entries under directory `root` (`relative` in the tree) written after `instant_ns`, as
/// tree paths: files and symlinks, and directories, whose ctime also moves when one is moved
/// in whole or has an entry added or removed. A directory moved in keeps its children's
/// ctimes (PULL-19), so a newer directory counts with everything inside it.
fn written_since(root: &Path, relative: &Path, instant_ns: u128) -> Vec<PathBuf> {
    let mut newer = Vec::new();
    let mut walker = WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else {
            continue;
        };
        if !written_after(entry.path(), instant_ns) {
            continue;
        }
        if entry.file_type().is_dir() {
            walker.skip_current_dir();
        }
        if let Ok(inner) = entry.path().strip_prefix(root) {
            newer.push(relative.join(inner));
        }
    }
    newer
}

/// Empty directory `root` (`relative` in the tree) of everything but the entries written
/// after `instant_ns` (see [`written_since`]; a newer directory stays whole) and the
/// directories holding them; with an undo record, what goes is kept there.
fn remove_all_but_newer(
    root: &Path,
    relative: &Path,
    instant_ns: u128,
    keep: Option<(&Path, &Path)>,
) -> Result {
    // Parents before children, newer directories not descended into; acted on children first.
    let mut entries = Vec::new();
    let mut walker = WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else {
            continue;
        };
        if written_after(entry.path(), instant_ns) {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }
        entries.push(entry);
    }
    for entry in entries.iter().rev() {
        if entry.file_type().is_dir() {
            // Not empty: it holds a newer entry.
            let _ = fs::remove_dir(entry.path());
            continue;
        }
        match keep {
            Some((dir, _)) => {
                let inner = entry.path().strip_prefix(root).unwrap_or(entry.path());
                undo::keep(dir, &relative.join(inner), entry.path())?;
            }
            None => remove_existing(entry.path())?,
        }
    }
    Ok(())
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
    for entry in entries {
        let path = entry
            .io_context(|| format!("read journal directory {}", directory.display()))?
            .path();
        let matches = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| names_journal_of(name, op, workspace));
        if !matches {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).io_context(|| format!("read {}", path.display()))?,
        )
        .map_err(|error| {
            ForkspaceError::StateCorrupted(format!("invalid {} journal: {error}", op.as_str()))
        })?;
        // The file name is the lookup key; the recorded owner has the last word.
        let owner = value.get("workspace").and_then(|value| value.as_str());
        if owner == Some(workspace) && !is_terminal(&value) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Whether `name` is exactly the file name [`path`] gives a journal of `op` for `workspace`:
/// `<op>-<workspace>-<fork_time_ns>.json`. A prefix match would also take `a-b`'s journal
/// for `a` (DATA-02).
fn names_journal_of(name: &str, op: OperationKind, workspace: &str) -> bool {
    name.strip_prefix(op.as_str())
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_prefix(workspace))
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".json"))
        .is_some_and(|fork_time| {
            !fork_time.is_empty() && fork_time.bytes().all(|byte| byte.is_ascii_digit())
        })
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
        Some("merge" | "pull" | "restore") => phase == Some("done"),
        _ => false,
    }
}

/// Apply one step: delete the target, or stage the source entry beside it and rename it into
/// place so the target is never observed half-written.
///
/// Nothing is done through a parent that is no longer a directory of the tree (a symlink put
/// in its place could point anywhere). With a `guard`, a target that changed since planning
/// is left as it is and reported as held, as is a newer entry inside a directory the step
/// deletes or replaces.
pub(crate) fn apply_change(
    source_root: &Path,
    target_root: &Path,
    change: &FileChange,
    index: usize,
    operation: &str,
    undo: Option<&Path>,
    guard: Option<&StepGuard>,
) -> Result<Step> {
    let relative = change.path.as_path();
    let source = source_root.join(relative);
    let target = target_root.join(relative);
    let keep = undo.map(|dir| (dir, relative));
    if let Some(parent) = escaping_parent(target_root, relative) {
        // A deletion has nothing left to remove inside the tree.
        if change.kind == ChangeKind::Deleted {
            return Ok(Step::Done);
        }
        return refuse_escape(relative, &parent, guard.is_some());
    }
    if change.kind != ChangeKind::Deleted {
        if let Some(parent) = escaping_parent(source_root, relative) {
            return refuse_escape(relative, &parent, guard.is_some());
        }
    }
    // An addition over an entry the plan saw there (`pull --theirs`, `merge --overwrite-base`
    // on a path both sides added) replaces it like a modification. Any other addition checks
    // atomically when it installs (`install_new`); so does one whose target is gone, such as a
    // respelling whose deletion ran first.
    let replaces_planned = change.kind == ChangeKind::Added
        && guard.is_some_and(|guard| guard.planned_entry(relative))
        && fs::symlink_metadata(&target).is_ok();
    let added = change.kind == ChangeKind::Added && !replaces_planned;
    if let Some(guard) = guard {
        if !added && guard.changed_since_planning(relative, &target) {
            if already_applied(&source, &target, change.kind) {
                return Ok(Step::Done);
            }
            return Ok(Step::Held(vec![relative.to_path_buf()]));
        }
    }
    let planned_ns = guard.map(|guard| guard.planned_ns);
    if change.kind == ChangeKind::Deleted {
        return delete_entry(&target, relative, keep, planned_ns);
    }
    let vanished =
        |target: &Path| source_vanished(target, relative, added, operation, keep, planned_ns);
    let metadata = match fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if guard.is_some() && error.kind() == std::io::ErrorKind::NotFound => {
            return vanished(&target);
        }
        Err(error) => {
            return Err(ForkspaceError::Io {
                context: format!("inspect {}", source.display()),
                source: error,
            })
        }
    };
    ensure_parent_dir(&target)?;
    if metadata.is_dir() {
        return install_dir(&source, &metadata, &target, keep).map(|()| Step::Done);
    }
    let temporary = target.with_extension(format!("forkspace-{operation}-{index}.tmp"));
    if let Err(error) = stage_entry(&source, &metadata, &temporary) {
        if guard.is_some() && fs::symlink_metadata(&source).is_err() {
            remove_existing(&temporary)?;
            return vanished(&target);
        }
        return Err(error);
    }
    let existing = fs::symlink_metadata(&target).ok();
    if existing.as_ref().is_some_and(fs::Metadata::is_dir) {
        if added && guard.is_some() {
            // A directory appeared where the plan expected nothing.
            remove_existing(&temporary)?;
            return Ok(Step::Held(vec![relative.to_path_buf()]));
        }
        return replace_directory(&temporary, &target, relative, keep, planned_ns);
    }
    if added {
        return install_new(&temporary, &target, change, operation, guard.is_some());
    }
    install_over(&temporary, &target, keep).map(|()| Step::Done)
}

/// A guarded step whose source entry is gone since planning. For a pull the base deleted it:
/// a base deletion, applied like one to the entry the plan saw (PULL-22); an addition has
/// nothing to delete, and an entry that appeared there since is the workspace's. For a merge
/// the workspace deleted it after the plan read it: the base is left alone and the path held.
fn source_vanished(
    target: &Path,
    relative: &Path,
    added: bool,
    operation: &str,
    keep: Option<(&Path, &Path)>,
    planned_ns: Option<u128>,
) -> Result<Step> {
    if operation != "pull" {
        return Ok(Step::Held(vec![relative.to_path_buf()]));
    }
    if added {
        return Ok(if fs::symlink_metadata(target).is_ok() {
            Step::Held(vec![relative.to_path_buf()])
        } else {
            Step::Done
        });
    }
    delete_entry(target, relative, keep, planned_ns)
}

/// Delete `target`, keeping it in the undo record when there is one. A directory with
/// entries written after `planned_ns` keeps those (and the directories holding them).
fn delete_entry(
    target: &Path,
    relative: &Path,
    keep: Option<(&Path, &Path)>,
    planned_ns: Option<u128>,
) -> Result<Step> {
    let Ok(metadata) = fs::symlink_metadata(target) else {
        return Ok(Step::Done);
    };
    if let Some(planned_ns) = planned_ns.filter(|_| metadata.is_dir()) {
        let newer = written_since(target, relative, planned_ns);
        if !newer.is_empty() {
            remove_all_but_newer(target, relative, planned_ns, keep)?;
            return Ok(Step::Held(newer));
        }
    }
    match keep {
        Some((dir, relative)) => undo::keep(dir, relative, target)?,
        None => remove_existing(target)?,
    }
    Ok(Step::Done)
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
    undo: Option<&Path>,
) -> Result<Step> {
    let staged = staging.join(&step.path);
    let target = target_root.join(&step.path);
    // A merge or pull keeps a newer entry and reports it; a restore fails.
    let holds = op != OperationKind::Restore;
    if let Some(parent) = escaping_parent(target_root, &step.path) {
        return refuse_escape(&step.path, &parent, holds);
    }
    let is_dir = |path: &Path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
    if is_dir(&staged) && is_dir(&target) {
        // Already a directory: put there by this step before a crash, or never changed.
        return Ok(Step::Done);
    }
    let unchanged = Fingerprint::of(&target) == step.expected;
    if !unchanged && !same_entry(&staged, &target) {
        if holds {
            return Ok(Step::Held(vec![step.path.clone()]));
        }
        return Err(ForkspaceError::InvalidArgument(format!(
            "{} changed while restoring; rerun the restore with --resume",
            step.path.display()
        )));
    }
    let metadata =
        fs::symlink_metadata(&staged).io_context(|| format!("inspect {}", staged.display()))?;
    ensure_parent_dir(&target)?;
    let temporary = target.with_extension(format!("forkspace-{}-{index}.tmp", op.as_str()));
    let keep = undo.map(|dir| (dir, step.path.as_path()));
    if metadata.is_dir() {
        // A directory to put back where a file or symlink is (a restore, CKPT-10): its
        // entries are steps of their own.
        return install_staged_dir(&metadata, &temporary, &target, keep).map(|()| Step::Done);
    }
    stage_entry(&staged, &metadata, &temporary)?;
    if fs::symlink_metadata(&target).is_ok_and(|existing| existing.is_dir()) {
        // A file or symlink where a directory is now (FS-10): swapped whole.
        return replace_directory_now(&temporary, &target, keep).map(|()| Step::Done);
    }
    install_over(&temporary, &target, keep).map(|()| Step::Done)
}

/// Make `target` a directory with the permissions in `metadata`. A file or symlink there is
/// swapped for an empty directory made at `temporary` (kept when there is an undo record), so
/// the path is never missing; an existing directory stays as it is.
fn install_staged_dir(
    metadata: &fs::Metadata,
    temporary: &Path,
    target: &Path,
    keep: Option<(&Path, &Path)>,
) -> Result {
    match fs::symlink_metadata(target) {
        Ok(existing) if existing.is_dir() => return Ok(()),
        Err(_) => {
            // Missing: nothing to swap.
            fs::create_dir(target).io_context(|| format!("create {}", target.display()))?;
        }
        Ok(_) => {
            remove_existing(temporary)?;
            fs::create_dir(temporary).io_context(|| format!("create {}", temporary.display()))?;
            match swap_with_directory(temporary, target)? {
                FlaggedRename::Done => match keep {
                    Some((dir, relative)) => undo::keep(dir, relative, temporary)?,
                    None => remove_existing(temporary)?,
                },
                _ => {
                    if let Some((dir, relative)) = keep {
                        undo::keep_copy(dir, relative, target)?;
                    }
                    remove_existing(target)?;
                    fs::rename(temporary, target)
                        .io_context(|| format!("install {}", target.display()))?;
                }
            }
        }
    }
    fs::set_permissions(target, metadata.permissions())
        .io_context(|| format!("set permissions on {}", target.display()))
}

/// Rename the staged `temporary` over `target`. With an undo record, the two are swapped so
/// the replaced entry can be kept; without swap support it is copied into the store first.
fn install_over(temporary: &Path, target: &Path, keep: Option<(&Path, &Path)>) -> Result {
    let install =
        || fs::rename(temporary, target).io_context(|| format!("install {}", target.display()));
    let Some((dir, relative)) = keep else {
        return install();
    };
    if fs::symlink_metadata(target).is_err() {
        return install();
    }
    match rename_exchange(temporary, target)
        .io_context(|| format!("swap {} into place", target.display()))?
    {
        FlaggedRename::Done => undo::keep(dir, relative, temporary),
        _ => {
            undo::keep_copy(dir, relative, target)?;
            install()
        }
    }
}

/// Put the staged entry where a directory is. A plain rename cannot replace a directory with
/// a non-directory, so the two are swapped atomically and the old tree is deleted from the
/// staging name; the path is never missing. Without swap support the directory is removed
/// first, as before. A directory holding entries written after `planned_ns` is not replaced:
/// it keeps those, loses the rest, and the step is held.
fn replace_directory(
    temporary: &Path,
    target: &Path,
    relative: &Path,
    keep: Option<(&Path, &Path)>,
    planned_ns: Option<u128>,
) -> Result<Step> {
    if let Some(planned_ns) = planned_ns {
        let newer = written_since(target, relative, planned_ns);
        if !newer.is_empty() {
            remove_existing(temporary)?;
            remove_all_but_newer(target, relative, planned_ns, keep)?;
            return Ok(Step::Held(newer));
        }
    }
    replace_directory_now(temporary, target, keep).map(|()| Step::Done)
}

fn replace_directory_now(temporary: &Path, target: &Path, keep: Option<(&Path, &Path)>) -> Result {
    match swap_with_directory(temporary, target)? {
        FlaggedRename::Done => match keep {
            Some((dir, relative)) => undo::keep(dir, relative, temporary),
            None => remove_existing(temporary),
        },
        _ => {
            if let Some((dir, relative)) = keep {
                undo::keep_copy(dir, relative, target)?;
            }
            remove_existing(target)?;
            fs::rename(temporary, target).io_context(|| format!("install {}", target.display()))
        }
    }
}

/// Swap `temporary` and `target`, one of them a directory, where the file system keeps a
/// swapped directory usable; elsewhere (fuse-overlayfs) report the swap unsupported, so the
/// caller removes the old entry first.
fn swap_with_directory(temporary: &Path, target: &Path) -> Result<FlaggedRename> {
    let parent = target.parent().unwrap_or(target);
    if !exchanges_directories(parent) {
        return Ok(FlaggedRename::Unsupported);
    }
    rename_exchange(temporary, target)
        .io_context(|| format!("swap {} into place", target.display()))
}

/// Install an entry the plan expected to be new without clobbering one that appeared since
/// planning. An identical entry is a step that already ran before a crash, so resuming
/// succeeds; anything else is held when `guarded`, else a conflict to resolve before
/// `--resume`.
fn install_new(
    temporary: &Path,
    target: &Path,
    change: &FileChange,
    operation: &str,
    guarded: bool,
) -> Result<Step> {
    let outcome = rename_noreplace(temporary, target)
        .io_context(|| format!("install {}", target.display()))?;
    match outcome {
        FlaggedRename::Done => Ok(Step::Done),
        FlaggedRename::Unsupported => {
            if fs::symlink_metadata(target).is_err() {
                fs::rename(temporary, target)
                    .io_context(|| format!("install {}", target.display()))?;
                return Ok(Step::Done);
            }
            appeared_since_planning(temporary, target, change, operation, guarded)
        }
        FlaggedRename::DestinationExists => {
            appeared_since_planning(temporary, target, change, operation, guarded)
        }
    }
}

fn appeared_since_planning(
    temporary: &Path,
    target: &Path,
    change: &FileChange,
    operation: &str,
    guarded: bool,
) -> Result<Step> {
    let identical = same_entry(temporary, target);
    remove_existing(temporary)?;
    if identical {
        return Ok(Step::Done);
    }
    if guarded {
        return Ok(Step::Held(vec![change.path.clone()]));
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
fn install_dir(
    source: &Path,
    metadata: &fs::Metadata,
    target: &Path,
    keep: Option<(&Path, &Path)>,
) -> Result {
    if fs::symlink_metadata(target).is_ok_and(|existing| !existing.is_dir()) {
        match keep {
            Some((dir, relative)) => undo::keep(dir, relative, target)?,
            None => remove_existing(target)?,
        }
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

    use super::{apply_change, apply_merged, Fingerprint, MergedStep, OperationKind, Step};
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

        let error = apply_change(
            source.path(),
            target.path(),
            &change,
            0,
            "merge",
            None,
            None,
        )
        .unwrap_err();
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

        apply_change(source.path(), target.path(), &change, 0, "pull", None, None).unwrap();
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
    }

    #[test]
    fn file_replaces_a_directory_in_one_step() {
        let (source, target) = dirs();
        fs::write(source.path().join("thing"), "now a file\n").unwrap();
        fs::create_dir_all(target.path().join("thing/nested")).unwrap();
        fs::write(target.path().join("thing/nested/old"), "old\n").unwrap();
        let change = FileChange::new(PathBuf::from("thing"), ChangeKind::Modified);

        apply_change(
            source.path(),
            target.path(),
            &change,
            3,
            "merge",
            None,
            None,
        )
        .unwrap();
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

    /// A directory outside both trees, with an entry the steps must never touch.
    fn outside() -> tempfile::TempDir {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("x.txt"), "outside\n").unwrap();
        outside
    }

    #[test]
    fn a_deletion_never_reaches_through_a_symlinked_parent() {
        let (source, target) = dirs();
        let outside = outside();
        std::os::unix::fs::symlink(outside.path(), target.path().join("d")).unwrap();
        let change = FileChange::new(PathBuf::from("d/x.txt"), ChangeKind::Deleted);

        let step =
            apply_change(source.path(), target.path(), &change, 0, "pull", None, None).unwrap();
        assert_eq!(step, Step::Done);
        assert_eq!(
            fs::read_to_string(outside.path().join("x.txt")).unwrap(),
            "outside\n"
        );
    }

    #[test]
    fn a_write_never_goes_through_a_symlinked_parent() {
        let (source, target) = dirs();
        let outside = outside();
        fs::create_dir(source.path().join("d")).unwrap();
        fs::write(source.path().join("d/x.txt"), "new\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), target.path().join("d")).unwrap();
        let change = FileChange::new(PathBuf::from("d/x.txt"), ChangeKind::Modified);

        apply_change(
            source.path(),
            target.path(),
            &change,
            0,
            "merge",
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(
            fs::read_to_string(outside.path().join("x.txt")).unwrap(),
            "outside\n"
        );
    }

    #[test]
    fn a_restore_never_writes_through_a_parent_swapped_for_a_symlink() {
        let (staging, target) = dirs();
        let outside = outside();
        fs::create_dir(staging.path().join("d")).unwrap();
        fs::write(staging.path().join("d/x.txt"), "checkpointed\n").unwrap();
        // Planned against the real directory; swapped before the step.
        std::os::unix::fs::symlink(outside.path(), target.path().join("d")).unwrap();
        let step = MergedStep {
            path: PathBuf::from("d/x.txt"),
            expected: Fingerprint::of(&target.path().join("d/x.txt")),
        };

        apply_merged(
            staging.path(),
            target.path(),
            &step,
            0,
            OperationKind::Restore,
            None,
        )
        .unwrap_err();
        assert_eq!(
            fs::read_to_string(outside.path().join("x.txt")).unwrap(),
            "outside\n"
        );
    }
}
