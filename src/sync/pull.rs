//! Pull: bring base-side changes made since the fork into a workspace, then re-baseline.
//!
//! The lifecycle is [`build`] → [`start`] (journaled apply) → [`finish`] (new fork-time
//! manifest) → [`complete`] (journal removed). [`resume_apply`] picks up an interrupted apply.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

use crate::backend::overlay::frozen;
use crate::backend::{select_backend, BackendState};
use crate::changes::content::files_differ;
use crate::changes::filter::ChangeFilter;
use crate::changes::manifest::{self, ManifestKind, ManifestRow, ManifestStatus};
use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::sync::ancestor;
use crate::sync::journal::{
    self, BaseChange, MergedStep, OperationJournal, OperationKind, OperationPhase, Outcome,
    PullRecord, Pulled, StepGuard,
};
use crate::sync::three_way::{reconcile, Favor, Policy};
use crate::sync::undo::Identity;
use crate::sync::{display_paths, is_transferable, order_respellings, render_plan};
use crate::util::fs::{ctime_ns, mode_bits, mtime_ns, same_contents};
use crate::util::walk::Entry;
use crate::util::{now_rfc3339, IoContext, WalkDirExt};
use crate::workspace::Workspace;

/// The base-side changes a pull would copy into a workspace.
#[derive(Debug, Clone, Serialize)]
pub struct PullPlan {
    pub workspace: String,
    /// The base directory changes are copied from.
    pub source: PathBuf,
    /// The workspace view changes land in.
    pub target: PathBuf,
    pub changes: Vec<FileChange>,
    /// Paths both sides changed that could not be combined line by line.
    pub conflicts: Vec<PathBuf>,
    /// Paths both sides changed whose edits were combined line by line.
    pub auto_merged: Vec<PathBuf>,
    /// Paths whose overlapping hunks are written between conflict markers, to resolve in the
    /// workspace.
    pub marked: Vec<PathBuf>,
    /// Journal steps installing the staged line-level merges.
    #[serde(skip)]
    pub merged_steps: Vec<MergedStep>,
    /// Paths both sides changed to the same result; nothing to copy.
    pub convergent: Vec<PathBuf>,
    /// Directories the base deleted that are kept because the workspace changed something
    /// inside them; the base's deletions inside still apply.
    pub kept: Vec<PathBuf>,
    /// The view entries the changes replace, as planning found them.
    #[serde(skip)]
    pub guard: StepGuard,
    /// Every base change the plan considered, with or without a step.
    #[serde(skip)]
    pub base_delta: Vec<BaseChange>,
    /// Base changes a catch-up round leaves for the next pull: they conflict with the
    /// workspace.
    #[serde(skip)]
    pub deferred: Vec<PathBuf>,
}

/// Compute the base-side delta against the fork-time manifest.
pub fn base_changes(workspace: &Workspace) -> Result<Vec<FileChange>> {
    if workspace.manifest_status != ManifestStatus::Ready
        || !manifest::manifest_path(&workspace.name).exists()
    {
        return Err(ForkspaceError::InvalidArgument(format!(
            "pull unavailable: workspace '{}' has no ready fork-time manifest; merge or preserve its changes, then re-fork",
            workspace.name
        )));
    }
    if !workspace.base.is_dir() {
        return Err(ForkspaceError::InvalidArgument(format!(
            "base directory for '{}' is missing; restore it before pulling",
            workspace.name
        )));
    }
    let manifest = manifest::Manifest::open(&workspace.name)?;
    let mut rows = BTreeMap::new();
    for row in manifest.iter() {
        // Encoding keeps `node_modules` and `.git` as they are, so rows inside them (most of
        // a JavaScript project) are skipped before they are decoded.
        if row
            .path
            .split('/')
            .any(|component| component == "node_modules" || component == ".git")
        {
            continue;
        }
        rows.insert(manifest::decode_path(row.path)?, row.to_row());
    }

    let mut present = BTreeSet::new();
    let mut changes = Vec::new();
    // Runtime and internal directories (`node_modules`, `.git`) never transfer: skip them.
    let walker = WalkDir::new(&workspace.base)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            !entry.file_type().is_dir()
                || entry
                    .path()
                    .strip_prefix(&workspace.base)
                    .is_ok_and(|relative| is_transferable(ChangeFilter::class(relative)))
        });
    for entry in walker {
        let entry = entry.walk_context(&workspace.base)?;
        let relative = entry
            .path()
            .strip_prefix(&workspace.base)
            .map_err(|_| {
                ForkspaceError::StateFailed("base scan escaped the workspace origin".to_string())
            })?
            .to_path_buf();
        present.insert(relative.clone());
        if !is_transferable(ChangeFilter::class(&relative)) {
            continue;
        }
        match rows.get(&relative) {
            Some(row) if base_unchanged(workspace, &relative, entry.path(), row) => {}
            Some(_) => changes.push(FileChange::new(relative, ChangeKind::Modified)),
            None => changes.push(FileChange::new(relative, ChangeKind::Added)),
        }
    }
    let has_fork_copy = ancestor::has_copy(workspace);
    changes.extend(
        rows.into_iter()
            .filter(|(path, row)| {
                !present.contains(path)
                    && is_transferable(ChangeFilter::class(path))
                    // A dirty row was recorded from the view after the workspace wrote it; if
                    // the fork-time copy never had the path, the base never did either.
                    && !(row.dirty && has_fork_copy && ancestor::entry(workspace, path).is_none())
            })
            .map(|(path, _)| FileChange::new(path, ChangeKind::Deleted)),
    );
    // Raw byte order rather than `Path`'s component-wise order.
    changes.sort_by(|a, b| {
        a.path
            .as_os_str()
            .as_encoded_bytes()
            .cmp(b.path.as_os_str().as_encoded_bytes())
    });
    Ok(changes)
}

/// Whether the base entry `path` (`relative` in the workspace) still holds what its
/// fork-time row `row` describes, so it is not a base change.
///
/// A dirty row was recorded after the workspace wrote the path, so it describes the
/// workspace's version, not the base's: the base entry is unchanged if nothing wrote it since
/// the fork (PULL-08). A clean row that no longer matches, or an entry written since the fork,
/// is compared byte for byte with the fork-time copy: only its mtime may have moved (a
/// checkout round trip, a rebase, `touch`), which changes nothing (PULL-11). Without a
/// fork-time copy such an entry counts as changed. As `plan::base_holds_fork_version`, on the
/// one `lstat` the scan needs.
fn base_unchanged(workspace: &Workspace, relative: &Path, path: &Path, row: &ManifestRow) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    let entry = Entry::of(&metadata);
    if row.kind == ManifestKind::Dir {
        return metadata.is_dir() && (row.dirty || row.mode == 0 || entry.mode == row.mode);
    }
    let untouched = !manifest::written_after_fork(&entry, workspace.fork_time_ns);
    let metadata_matches =
        row.dirty || (row.matches(path, &entry) && mtime_ns(&metadata) == row.mtime_ns);
    if untouched && metadata_matches {
        return true;
    }
    ancestor::entry(workspace, relative).is_some_and(|fork| !files_differ(&fork, path))
}

/// The ctime after which a base entry may have changed since the pull journaled with
/// `guard` read the base.
fn base_read_at(guard: Option<&StepGuard>) -> i128 {
    let read = guard.map_or(0, |guard| guard.base_ns.unwrap_or(guard.planned_ns));
    i128::try_from(read).unwrap_or(i128::MAX)
}

/// Whether the base changed after the pull journaled with `guard` read it: a transferable
/// entry, or a directory (which a deletion or rename inside also touches), written later.
pub fn base_moved_since(workspace: &Workspace, guard: Option<&StepGuard>) -> bool {
    let since = base_read_at(guard);
    WalkDir::new(&workspace.base)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || entry
                    .path()
                    .strip_prefix(&workspace.base)
                    .is_ok_and(|relative| is_transferable(ChangeFilter::class(relative)))
        })
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            fs::symlink_metadata(entry.path()).is_ok_and(|metadata| ctime_ns(&metadata) > since)
        })
}

/// Every base change the pull journaled as `journal` planned or stepped, in this round and the
/// earlier ones, with what came of it.
pub fn history(journal: &OperationJournal) -> BTreeMap<PathBuf, Pulled> {
    let mut history: BTreeMap<PathBuf, Pulled> = journal
        .earlier
        .iter()
        .map(|pulled| (pulled.path.clone(), pulled.clone()))
        .collect();
    let steps: BTreeMap<&Path, usize> = journal
        .changes
        .iter()
        .enumerate()
        .map(|(index, change)| (change.path.as_path(), index))
        .collect();
    let merged: BTreeSet<&Path> = journal
        .merged
        .iter()
        .map(|step| step.path.as_path())
        .collect();
    let held: BTreeSet<&Path> = journal.held.iter().map(PathBuf::as_path).collect();
    let deferred: BTreeSet<&Path> = journal.deferred.iter().map(PathBuf::as_path).collect();
    let gone = |path: &Path| fs::symlink_metadata(journal.target.join(path)).is_err();
    let outcome = |path: &Path| {
        if deferred.contains(path) {
            return Outcome::Deferred;
        }
        if held.contains(path) {
            return Outcome::Held;
        }
        match steps.get(path) {
            Some(&index) if index < journal.next => {
                if merged.contains(path) {
                    Outcome::Merged
                } else if journal.changes[index].kind == ChangeKind::Deleted {
                    Outcome::Deleted
                } else {
                    match journal.installed.get(index).copied().flatten() {
                        Some(identity) => Outcome::Installed(identity),
                        // Its source vanished: applied as a deletion.
                        None if gone(path) => Outcome::Deleted,
                        None => Outcome::Unstepped,
                    }
                }
            }
            Some(_) => Outcome::Unstepped,
            // Inside a directory a step deleted or replaced whole.
            None if gone(path)
                && path.ancestors().skip(1).any(|parent| {
                    steps
                        .get(parent)
                        .is_some_and(|&index| index < journal.next && !held.contains(parent))
                }) =>
            {
                Outcome::Deleted
            }
            None => Outcome::Unstepped,
        }
    };
    let planned = journal
        .base_delta
        .iter()
        .map(|change| (change.path.as_path(), change.kind))
        .chain(
            journal
                .changes
                .iter()
                .map(|change| (change.path.as_path(), change.kind)),
        );
    let mut seen = BTreeSet::new();
    for (path, kind) in planned {
        if !seen.insert(path) {
            continue;
        }
        history.insert(
            path.to_path_buf(),
            Pulled {
                path: path.to_path_buf(),
                kind,
                outcome: outcome(path),
            },
        );
    }
    history
}

/// Whether the view's `path` is still exactly what the pull put there: no workspace change.
fn untouched(history: &BTreeMap<PathBuf, Pulled>, view: &Path, path: &Path) -> bool {
    match history.get(path).map(|pulled| pulled.outcome) {
        Some(Outcome::Installed(identity)) => Identity::of(&view.join(path)) == Some(identity),
        Some(Outcome::Deleted) => fs::symlink_metadata(view.join(path)).is_err(),
        _ => false,
    }
}

/// Plan a catch-up round: the base changes made after the pull journaled as `previous` read
/// the base (while it applied, or while it was interrupted), so they are pulled before the
/// workspace is re-baselined on the base (PULL-10).
///
/// A base change no round planned yet is new, wherever it is (a directory renamed in brings
/// entries with old ctimes, PULL-21); one a round planned is new again only if the base wrote
/// it since, or deleted what it had added or re-created what it had deleted. A path the pull
/// changed that the base has since put back is put back in the view too. What the pull put in
/// the view and nobody touched since is not a workspace change. A round never takes the base's
/// side of a conflict and never writes conflict markers: such a path is deferred, left for the
/// next pull (PULL-20).
pub fn build_since(
    workspace: &Workspace,
    previous: &OperationJournal,
    install_runs: bool,
) -> Result<PullPlan> {
    let history = history(previous);
    let since = base_read_at(previous.guard.as_ref());
    let written_since =
        |path: &Path| fs::symlink_metadata(path).is_ok_and(|metadata| ctime_ns(&metadata) > since);
    let backend = select_backend(Some(workspace.backend_kind))?;
    let base_ns = crate::util::fs::ctime_clock_ns();
    let all = backend.base_changes(workspace)?;
    let current: BTreeSet<PathBuf> = all.iter().map(|change| change.path.clone()).collect();
    let mut theirs_delta: Vec<FileChange> = all
        .into_iter()
        .filter(|change| match history.get(&change.path) {
            None => true,
            Some(seen) if change.kind == ChangeKind::Deleted => seen.kind != ChangeKind::Deleted,
            Some(seen) => {
                seen.kind == ChangeKind::Deleted
                    || written_since(&workspace.base.join(&change.path))
            }
        })
        .collect();
    for (path, seen) in &history {
        if current.contains(path) {
            continue;
        }
        let in_base = fs::symlink_metadata(workspace.base.join(path)).is_ok();
        match seen.outcome {
            Outcome::Installed(_) => {}
            Outcome::Deleted if in_base => {}
            _ => continue,
        }
        let kind = match (
            in_base,
            fs::symlink_metadata(workspace.view_path.join(path)),
        ) {
            (false, _) => ChangeKind::Deleted,
            (true, Ok(_)) => ChangeKind::Modified,
            (true, Err(_)) => ChangeKind::Added,
        };
        theirs_delta.push(FileChange::new(path.clone(), kind));
    }
    // What the view already has exactly as the base does needs nothing (a write the slack
    // counts as late, or one the pull already brought in).
    theirs_delta.retain(|change| {
        !equivalent(
            &workspace.base.join(&change.path),
            &workspace.view_path.join(&change.path),
        )
    });
    theirs_delta.sort_by(|a, b| {
        a.path
            .as_os_str()
            .as_encoded_bytes()
            .cmp(b.path.as_os_str().as_encoded_bytes())
    });
    let mut guard = StepGuard::capture(
        &workspace.view_path,
        theirs_delta.iter().map(|change| change.path.as_path()),
    );
    guard.base_ns = Some(base_ns);
    let ours_delta: Vec<FileChange> = backend
        .transferable_changes(workspace)?
        .into_iter()
        .filter(|change| !untouched(&history, &workspace.view_path, &change.path))
        .collect();
    plan_changes(
        workspace,
        theirs_delta,
        ours_delta,
        Mode::Round,
        install_runs,
        guard,
    )
}

/// Plan a pull into `workspace`. With `theirs`, conflicting paths take the base version;
/// without it, any conflict is an error.
pub fn build(workspace: &Workspace, theirs: bool) -> Result<PullPlan> {
    build_with(workspace, theirs, false)
}

/// Like [`build`]; `install_runs` says the install step follows, so a lockfile both sides
/// changed can simply take the base's version.
pub fn build_with(workspace: &Workspace, theirs: bool, install_runs: bool) -> Result<PullPlan> {
    let backend = select_backend(Some(workspace.backend_kind))?;
    // Before the base is read: an entry written after this may be missing from the plan.
    let base_ns = crate::util::fs::ctime_clock_ns();
    let theirs_delta = backend.base_changes(workspace)?;
    // Before the view is read: a write after this point is caught when its step runs.
    let mut guard = StepGuard::capture(
        &workspace.view_path,
        theirs_delta.iter().map(|change| change.path.as_path()),
    );
    guard.base_ns = Some(base_ns);
    let ours_delta = backend.transferable_changes(workspace)?;
    crate::util::trace::time("plan", || {
        plan_changes(
            workspace,
            theirs_delta,
            ours_delta,
            Mode::Plan { theirs },
            install_runs,
            guard,
        )
    })
}

/// Plan a pull from already-collected base-side (`theirs_delta`) and workspace-side
/// (`ours_delta`) changes.
pub fn build_from_changes(
    workspace: &Workspace,
    theirs_delta: Vec<FileChange>,
    ours_delta: Vec<FileChange>,
    theirs: bool,
) -> Result<PullPlan> {
    let guard = StepGuard::capture(
        &workspace.view_path,
        theirs_delta.iter().map(|change| change.path.as_path()),
    );
    plan_changes(
        workspace,
        theirs_delta,
        ours_delta,
        Mode::Plan { theirs },
        false,
        guard,
    )
}

/// What a plan is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The pull the user asked for; with `theirs`, conflicts take the base's version, else
    /// any conflict fails the plan.
    Plan { theirs: bool },
    /// A catch-up round ([`build_since`]): conflicts and overlapping edits are deferred.
    Round,
}

fn plan_changes(
    workspace: &Workspace,
    theirs_delta: Vec<FileChange>,
    ours_delta: Vec<FileChange>,
    mode: Mode,
    install_runs: bool,
    guard: StepGuard,
) -> Result<PullPlan> {
    let theirs = mode == Mode::Plan { theirs: true };
    let round = mode == Mode::Round;
    // What forks leave out never moves either way (nested worktrees, `.forkspaceexclude`).
    let exclusions = crate::exclude::Exclusions::of(&workspace.base);
    let theirs_delta: Vec<FileChange> = theirs_delta
        .into_iter()
        .filter(|change| !exclusions.is_excluded(&change.path, false))
        .collect();
    let ours_delta: Vec<FileChange> = ours_delta
        .into_iter()
        .filter(|change| !exclusions.is_excluded(&change.path, false))
        .collect();
    let base_delta: Vec<BaseChange> = theirs_delta
        .iter()
        .map(|change| BaseChange {
            path: change.path.clone(),
            kind: change.kind,
        })
        .collect();
    let ours: BTreeSet<_> = ours_delta
        .into_iter()
        .filter(|change| is_transferable(change.class))
        .map(|change| change.path)
        .collect();
    let mut changes = Vec::new();
    let mut both_changed = Vec::new();
    let mut convergent = Vec::new();
    let view = &workspace.view_path;
    // Applying a base-side deletion or replacement of a directory removes the view's whole
    // directory, so it must not run over a workspace change that is still inside (DATA-01).
    // Descendants of `dir` sort right after it in component order.
    let holds_ours = |dir: &Path| {
        ours.range::<Path, _>((Bound::Excluded(dir), Bound::Unbounded))
            .take_while(|path| path.starts_with(dir))
            .any(|path| fs::symlink_metadata(view.join(path)).is_ok())
    };
    let is_real_dir = |path: &Path| fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir());
    let mut kept = Vec::new();
    let mut replaced = Vec::new();
    // Paths the base itself changed: a view entry there is replaced by the base's step.
    let base_changed: BTreeSet<PathBuf> = theirs_delta
        .iter()
        .map(|change| change.path.clone())
        .collect();
    for change in theirs_delta {
        // A deletion of a path the base still resolves is a rename between spellings that a
        // case- or normalization-insensitive volume (APFS) treats as one name (PULL-17). Its
        // directory is still a directory in the base, so the check below would let the
        // deletion remove the workspace's changes inside it: a conflict.
        if change.kind == ChangeKind::Deleted
            && fs::symlink_metadata(workspace.base.join(&change.path)).is_ok()
            && (ours.contains(&change.path) || holds_ours(&change.path))
        {
            replaced.push(change);
            continue;
        }
        if is_real_dir(&view.join(&change.path))
            && !is_real_dir(&workspace.base.join(&change.path))
            && holds_ours(&change.path)
        {
            // A deleted directory stays for what the workspace put in it (the deletions the
            // base made inside are their own steps); one the base replaced with a file or a
            // symlink conflicts with them.
            if change.kind == ChangeKind::Deleted {
                kept.push(change.path);
            } else {
                replaced.push(change);
            }
            continue;
        }
        if !ours.contains(&change.path) {
            changes.push(change);
        } else if equivalent(
            &workspace.base.join(&change.path),
            &workspace.view_path.join(&change.path),
        ) {
            convergent.push(change.path);
        } else {
            both_changed.push(change);
        }
    }

    // Nothing is kept under a directory the base replaced: taking the base's entry removes it.
    kept.retain(|path: &PathBuf| !replaced.iter().any(|change| path.starts_with(&change.path)));

    // The workspace put a file or symlink where the base still has a directory, and the base
    // added or changed something inside it (PULL-16): there is no directory in the view to
    // write it into. A conflict on that path; `--theirs` takes the base's directory whole.
    // Where the base turned the entry into a directory itself, its own step makes the
    // directory (PULL-24); a workspace change to that entry is a conflict on the entry.
    let mut blocked = BTreeSet::new();
    for list in [&mut changes, &mut both_changed] {
        list.retain(|change| {
            let parent = (change.kind != ChangeKind::Deleted)
                .then(|| {
                    replaced_by_the_workspace(view, &workspace.base, &change.path, &base_changed)
                })
                .flatten();
            parent.map(|parent| blocked.insert(parent)).is_none()
        });
    }
    let mut guard = guard;
    if theirs {
        for parent in &blocked {
            take_base_directory(workspace, parent, &mut changes, &mut guard);
        }
    }

    let staging = journal::staging_dir(&journal::path(
        OperationKind::Pull,
        &workspace.name,
        workspace.fork_time_ns,
    ));
    let _ = fs::remove_dir_all(&staging);
    let paths: Vec<PathBuf> = both_changed
        .iter()
        .map(|change| change.path.clone())
        .collect();
    let policy = Policy {
        favor: if theirs {
            Favor::Project
        } else {
            Favor::Neither
        },
        allow_markers: !round,
        install_runs,
    };
    let reconciled = reconcile(workspace, &paths, view, view, &staging, policy)?;
    let mut conflicts = reconciled.whole_file;
    conflicts.extend(replaced.iter().map(|change| change.path.clone()));
    conflicts.extend(blocked);
    conflicts.sort();
    convergent.sort();
    let mut deferred = Vec::new();
    if round {
        // Left for the next pull, which reports them like any conflict.
        deferred = conflicts;
        deferred.extend(reconciled.overlapping);
        deferred.sort();
        conflicts = Vec::new();
        both_changed.retain(|change| deferred.binary_search(&change.path).is_err());
        replaced.clear();
    } else if !conflicts.is_empty() && !theirs {
        let _ = fs::remove_dir_all(&staging);
        return Err(ForkspaceError::PullConflict {
            paths: display_paths(&conflicts),
        });
    }
    changes.extend(both_changed);
    changes.extend(replaced);
    // Raw byte order, as `base_changes` produces.
    changes.sort_by(|a, b| {
        a.path
            .as_os_str()
            .as_encoded_bytes()
            .cmp(b.path.as_os_str().as_encoded_bytes())
    });
    order_respellings(&workspace.view_path, &mut changes);
    Ok(PullPlan {
        workspace: workspace.name.clone(),
        source: workspace.base.clone(),
        target: workspace.view_path.clone(),
        changes,
        conflicts,
        auto_merged: reconciled.auto_merged,
        marked: reconciled.marked,
        merged_steps: reconciled.steps,
        convergent,
        kept,
        guard,
        base_delta,
        deferred,
    })
}

/// The topmost parent of `relative` that the view has as a file or symlink while the base has
/// a directory there, and that the base did not change itself (`base_changed`).
fn replaced_by_the_workspace(
    view: &Path,
    base: &Path,
    relative: &Path,
    base_changed: &BTreeSet<PathBuf>,
) -> Option<PathBuf> {
    let mut parents: Vec<&Path> = relative.ancestors().skip(1).collect();
    parents.reverse();
    parents
        .into_iter()
        .filter(|parent| !parent.as_os_str().is_empty() && !base_changed.contains(*parent))
        .find(|parent| {
            fs::symlink_metadata(view.join(parent)).is_ok_and(|metadata| !metadata.is_dir())
                && fs::symlink_metadata(base.join(parent)).is_ok_and(|metadata| metadata.is_dir())
        })
        .map(Path::to_path_buf)
}

/// Plan to replace the view's entry at `parent` with the base's directory and everything
/// transferable inside it.
fn take_base_directory(
    workspace: &Workspace,
    parent: &Path,
    changes: &mut Vec<FileChange>,
    guard: &mut StepGuard,
) {
    let planned: BTreeSet<PathBuf> = changes.iter().map(|change| change.path.clone()).collect();
    guard.targets.push(journal::PlannedTarget {
        path: parent.to_path_buf(),
        state: journal::TargetState::of(&workspace.view_path.join(parent)),
    });
    changes.push(FileChange::new(parent.to_path_buf(), ChangeKind::Modified));
    let root = workspace.base.join(parent);
    let inside = WalkDir::new(&root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry
                .path()
                .strip_prefix(&workspace.base)
                .is_ok_and(|relative| is_transferable(ChangeFilter::class(relative)))
        })
        .filter_map(|entry| entry.ok());
    for entry in inside {
        let Ok(relative) = entry.path().strip_prefix(&workspace.base) else {
            continue;
        };
        if !planned.contains(relative) {
            changes.push(FileChange::new(relative.to_path_buf(), ChangeKind::Added));
        }
    }
}

/// Human-readable plan, one line per change plus the conflicts resolved toward the base.
pub fn render(plan: &PullPlan) -> String {
    render_plan(
        format!(
            "pull plan for '{}': {} files",
            plan.workspace,
            plan.changes.len()
        ),
        &plan.changes,
        |kind| match kind {
            ChangeKind::Added => "add",
            ChangeKind::Modified => "update",
            ChangeKind::Deleted => "delete",
        },
        &[
            ("merged line by line", &plan.auto_merged),
            ("conflict markers to resolve", &plan.marked),
            ("taking base for conflicts", &plan.conflicts),
            ("kept for workspace changes", &plan.kept),
        ],
    )
}

/// Journal and apply `plan`, returning the journal path for [`finish`] and [`complete`].
pub fn start(workspace: &Workspace, plan: &PullPlan) -> Result<PathBuf> {
    start_round(workspace, plan, None)
}

/// [`start`] for a catch-up round after the round journaled as `previous`, whose record the
/// new journal (at the same path) carries on.
pub fn start_round(
    workspace: &Workspace,
    plan: &PullPlan,
    previous: Option<&OperationJournal>,
) -> Result<PathBuf> {
    let _apply = crate::util::trace::span("apply");
    align_convergent_times(plan);
    let (earlier, held) = previous.map_or_else(Default::default, |previous| {
        (
            history(previous).into_values().collect(),
            previous.held.clone(),
        )
    });
    journal::start_pull(
        &workspace.name,
        plan.source.clone(),
        plan.target.clone(),
        plan.changes.clone(),
        workspace.fork_time_ns,
        plan.merged_steps.clone(),
        plan.guard.clone(),
        PullRecord {
            base_delta: plan.base_delta.clone(),
            earlier,
            deferred: plan.deferred.clone(),
            held,
        },
    )
}

/// Give each file both sides changed identically the base's mtime. The re-baselined manifest
/// is built from the base, so a workspace copy with its own mtime would otherwise show as a
/// change with no difference. Best effort: a file left alone is only a spurious diff entry.
fn align_convergent_times(plan: &PullPlan) {
    for path in &plan.convergent {
        let Ok(metadata) = fs::symlink_metadata(plan.source.join(path)) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if metadata.is_file() {
            let _ = fs::File::open(plan.target.join(path))
                .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)));
        }
    }
}

/// Finish applying the interrupted pull of `workspace`, if its steps are not all applied yet.
pub fn resume_apply(workspace: &Workspace) -> Result<PathBuf> {
    let path = find_open_journal(&workspace.name)?.ok_or_else(|| {
        ForkspaceError::InvalidArgument(format!(
            "no interrupted pull journal for '{}'",
            workspace.name
        ))
    })?;
    let journal = journal::load(&path)?;
    if journal.op != OperationKind::Pull {
        return Err(ForkspaceError::StateCorrupted(format!(
            "{} is not a pull journal",
            path.display()
        )));
    }
    if matches!(
        journal.phase,
        OperationPhase::Planned | OperationPhase::Applying
    ) {
        journal::apply_remaining(&path)?;
    }
    Ok(path)
}

/// Re-baseline `workspace` on the base as the pull journaled at `path` brought it into the
/// view. The caller must persist state before [`complete`] removes the journal. Returns the
/// base changes left for the next pull: made after the pull last read the base, or deferred
/// by a catch-up round.
pub fn finish(workspace: &mut Workspace, path: &Path) -> Result<Vec<PathBuf>> {
    journal::set_phase(path, OperationPhase::Rebaselining)?;
    if matches!(
        workspace.backend_state,
        BackendState::MacClone { .. } | BackendState::CowDir { .. }
    ) {
        return finish_snapshot(workspace, &journal::load(path)?);
    }
    // The new ancestor is the current origin, not the possibly still-edited workspace.
    // This preserves ours-only edits in subsequent diff and merge results. A sealed overlay
    // measures its changes against a frozen copy, so it gets a fresh one of the origin; the
    // view keeps its mounted lower layer, and the pulled files in its upper layer now match
    // the ancestor.
    let ancestor = match &mut workspace.backend_state {
        BackendState::Overlay(layers) if layers.lower.is_some() => {
            // The full frozen copy the baseline builds on: a delta's base, else the baseline.
            let full = layers
                .ancestor_base
                .clone()
                .or_else(|| layers.ancestor.clone())
                .or_else(|| layers.lower.clone())
                .expect("a sealed overlay has a lower layer");
            match frozen::freeze_delta(&workspace.base, &full)? {
                Some(frozen::Delta::Unchanged) => {
                    layers.ancestor = (layers.lower.as_ref() != Some(&full)).then(|| full.clone());
                    layers.ancestor_base = None;
                    full
                }
                // Only what changed since `full` is recorded; no whole new copy is made.
                Some(frozen::Delta::Made(delta)) => {
                    layers.ancestor = Some(delta.clone());
                    layers.ancestor_base = Some(full);
                    delta
                }
                None => {
                    let frozen = frozen::freeze(&workspace.base, frozen::Basis::Project)?;
                    layers.ancestor =
                        (layers.lower.as_ref() != Some(&frozen.tree)).then(|| frozen.tree.clone());
                    layers.ancestor_base = None;
                    frozen.tree
                }
            }
        }
        BackendState::Overlay(_) | BackendState::MacClone { .. } | BackendState::CowDir { .. } => {
            workspace.base.clone()
        }
    };
    let fork_time_ns = crate::util::fs::ctime_clock_ns();
    // A frozen ancestor carries its manifest; anything else is walked.
    let cache = frozen::manifest_cache(&ancestor);
    let copied = matches!(&workspace.backend_state, BackendState::Overlay(layers) if layers.lower.is_some())
        && cache.exists()
        && manifest::install_copy(&workspace.name, &cache).is_ok();
    if !copied {
        manifest::build(&workspace.name, &ancestor, fork_time_ns)?;
    }
    workspace.fork_time_ns = fork_time_ns;
    workspace.manifest_generation = workspace.manifest_generation.saturating_add(1);
    workspace.last_pull = Some(now_rfc3339());
    workspace.manifest_status = ManifestStatus::Ready;
    Ok(Vec::new())
}

/// [`finish`] for a snapshot workspace: a new fork-time copy and manifest of the base, with
/// every entry the base changed after the pull last read it (PULL-23), or that a round
/// deferred, recorded as the view has it instead (see [`settle_late_changes`]).
fn finish_snapshot(workspace: &mut Workspace, journal: &OperationJournal) -> Result<Vec<PathBuf>> {
    let staging = ancestor::stage(&workspace.name, &workspace.base)?;
    let fork_time_ns = crate::util::fs::ctime_clock_ns();
    let settled = manifest::rows_of_tree(&workspace.base, fork_time_ns).and_then(|mut rows| {
        let left = settle_late_changes(workspace, journal, &staging, &mut rows, fork_time_ns)?;
        Ok((rows, left))
    });
    let (rows, left) = match settled {
        Ok(settled) => settled,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    ancestor::install(&workspace.name, &staging)?;
    manifest::install_rows(&workspace.name, rows)?;
    workspace.fork_time_ns = fork_time_ns;
    workspace.manifest_generation = workspace.manifest_generation.saturating_add(1);
    workspace.last_pull = Some(now_rfc3339());
    workspace.manifest_status = ManifestStatus::Ready;
    Ok(left)
}

/// Where the new fork-time entry of a path that changed late comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Baseline {
    /// The view's entry, which is the base's as the pull brought it in.
    View,
    /// The previous fork-time entry: the pull never read the base's change.
    Previous,
    /// No entry: the base had none when the pull read it.
    Absent,
    /// The base's entry now.
    Base,
}

/// Make the new baseline (`rows` and the staged fork-time copy `staging`) describe the base
/// as the pull read it, not as it is now, wherever it changed after the pull last read it
/// (the journal's `base_ns`) or a round deferred a change. Recorded from the base, such an
/// entry would be in the baseline but not in the view, so it would look like the workspace's
/// own reversion and the next merge would undo it (PULL-23); recorded as read, it stays a base
/// change for the next pull. Also, a base file whose bytes are back to the view's version
/// with another mtime is recorded with the view's mtime, so it is no phantom change
/// (DIFF-12). Called before the workspace record moves to the new baseline.
fn settle_late_changes(
    workspace: &Workspace,
    journal: &OperationJournal,
    staging: &Path,
    rows: &mut Vec<ManifestRow>,
    fork_time_ns: u128,
) -> Result<Vec<PathBuf>> {
    let base = &workspace.base;
    let view = &workspace.view_path;
    let since = base_read_at(journal.guard.as_ref());
    let previous_fork = i128::try_from(workspace.fork_time_ns).unwrap_or(i128::MAX);
    let history = history(journal);
    let is_real_dir = |path: &Path| fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir());

    let mut late = BTreeSet::new();
    let mut late_dirs = Vec::new();
    let mut touched = Vec::new();
    let walker = WalkDir::new(base)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || entry
                    .path()
                    .strip_prefix(base)
                    .is_ok_and(|relative| is_transferable(ChangeFilter::class(relative)))
        });
    for entry in walker.filter_map(|entry| entry.ok()) {
        let Ok(relative) = entry.path().strip_prefix(base) else {
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let ctime = ctime_ns(&metadata);
        if ctime > since {
            if metadata.is_dir() {
                late_dirs.push(relative.to_path_buf());
            }
            if entry.depth() > 0 {
                late.insert(relative.to_path_buf());
            }
        } else if ctime > previous_fork && metadata.is_file() {
            touched.push(relative.to_path_buf());
        }
    }
    late.extend(
        history
            .values()
            .filter(|pulled| pulled.outcome == Outcome::Deferred)
            .map(|pulled| pulled.path.clone()),
    );
    // A directory changed in place lost the entries the view still has and the base not.
    for directory in &late_dirs {
        let Ok(children) = fs::read_dir(view.join(directory)) else {
            continue;
        };
        for child in children.flatten() {
            let relative = directory.join(child.file_name());
            if is_transferable(ChangeFilter::class(&relative))
                && fs::symlink_metadata(base.join(&relative)).is_err()
            {
                late.insert(relative);
            }
        }
    }
    // A directory on one side only comes with everything inside it.
    let mut subtrees = Vec::new();
    for path in &late {
        for (root, other) in [(base, view), (view, base)] {
            if !is_real_dir(&root.join(path)) || is_real_dir(&other.join(path)) {
                continue;
            }
            let inside = WalkDir::new(root.join(path))
                .min_depth(1)
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| entry.ok());
            for entry in inside {
                if let Ok(relative) = entry.path().strip_prefix(root) {
                    if is_transferable(ChangeFilter::class(relative)) {
                        subtrees.push(relative.to_path_buf());
                    }
                }
            }
        }
    }
    late.extend(subtrees);

    let mut by_path: BTreeMap<String, ManifestRow> = std::mem::take(rows)
        .into_iter()
        .map(|row| (row.path.clone(), row))
        .collect();
    // DIFF-12: same bytes as the view, another mtime.
    for path in &touched {
        let encoded = manifest::encode_path(path);
        let (Ok(in_view), Ok(in_base)) = (
            fs::symlink_metadata(view.join(path)),
            fs::symlink_metadata(base.join(path)),
        ) else {
            continue;
        };
        if in_view.is_file()
            && in_view.len() == in_base.len()
            && mode_bits(&in_view) == mode_bits(&in_base)
            && mtime_ns(&in_view) != mtime_ns(&in_base)
            && same_contents(&view.join(path), &base.join(path))
        {
            let row = manifest::row_of(path, &view.join(path), &Entry::of(&in_view), fork_time_ns)?;
            by_path.insert(encoded, row);
        }
    }

    let mut left = Vec::new();
    if !late.is_empty() {
        let previous_rows = manifest::Manifest::open(&workspace.name)?;
        let previous_copy = ancestor::dir(&workspace.name);
        let backend = select_backend(Some(workspace.backend_kind))?;
        let ours: BTreeSet<PathBuf> = backend
            .transferable_changes(workspace)?
            .into_iter()
            .filter(|change| !untouched(&history, view, &change.path))
            .map(|change| change.path)
            .collect();
        let is_ours = |path: &Path| path.ancestors().any(|path| ours.contains(path));
        for path in &late {
            let baseline = if !is_ours(path) {
                Baseline::View
            } else {
                match history.get(path) {
                    None => Baseline::Previous,
                    Some(pulled) => match pulled.outcome {
                        Outcome::Unstepped | Outcome::Deferred => Baseline::Previous,
                        Outcome::Deleted => Baseline::Absent,
                        Outcome::Held if pulled.kind == ChangeKind::Deleted => Baseline::Absent,
                        Outcome::Held | Outcome::Merged | Outcome::Installed(_) => Baseline::Base,
                    },
                }
            };
            let encoded = manifest::encode_path(path);
            let copied = staging.join(path);
            let source = match baseline {
                Baseline::Base => continue,
                Baseline::View => Some(view.join(path)),
                Baseline::Previous => Some(previous_copy.join(path)),
                Baseline::Absent => None,
            };
            match baseline {
                Baseline::View => match fs::symlink_metadata(view.join(path)) {
                    Ok(metadata) => {
                        let row = manifest::row_of(
                            path,
                            &view.join(path),
                            &Entry::of(&metadata),
                            fork_time_ns,
                        )?;
                        by_path.insert(encoded, row);
                    }
                    Err(_) => {
                        by_path.remove(&encoded);
                    }
                },
                Baseline::Previous => match previous_rows.get(&encoded) {
                    Some(row) => {
                        by_path.insert(encoded, row.to_row());
                    }
                    None => {
                        by_path.remove(&encoded);
                    }
                },
                Baseline::Absent => {
                    by_path.remove(&encoded);
                }
                Baseline::Base => {}
            }
            put_baseline_copy(source.as_deref(), &copied)?;
            let now = base.join(path);
            let differs = match &source {
                Some(source) => !is_real_dir(source) && !equivalent(&now, source),
                None => fs::symlink_metadata(&now).is_ok(),
            };
            if differs && !is_real_dir(&now) {
                left.push(path.clone());
            }
        }
    }
    *rows = by_path.into_values().collect();
    Ok(left)
}

/// Make `copied` (in a staged fork-time copy) what `source` is: a copy of its file or
/// symlink, a directory (its entries are put on their own), or nothing.
fn put_baseline_copy(source: Option<&Path>, copied: &Path) -> Result {
    let metadata = source.and_then(|source| fs::symlink_metadata(source).ok());
    let existing = fs::symlink_metadata(copied).ok();
    if let Some(existing) = &existing {
        let keep_dir = existing.is_dir() && metadata.as_ref().is_some_and(fs::Metadata::is_dir);
        if !keep_dir {
            let removed = if existing.is_dir() {
                fs::remove_dir_all(copied)
            } else {
                fs::remove_file(copied)
            };
            removed.io_context(|| format!("remove {}", copied.display()))?;
        }
    }
    let (Some(source), Some(metadata)) = (source, metadata) else {
        return Ok(());
    };
    crate::util::ensure_parent_dir(copied)?;
    if metadata.is_dir() {
        return fs::create_dir_all(copied).io_context(|| format!("create {}", copied.display()));
    }
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(source).io_context(|| format!("read {}", source.display()))?;
        return std::os::unix::fs::symlink(link, copied)
            .io_context(|| format!("create {}", copied.display()));
    }
    if !metadata.is_file() {
        return Ok(());
    }
    fs::copy(source, copied)
        .io_context(|| format!("copy {} -> {}", source.display(), copied.display()))?;
    let modified = metadata
        .modified()
        .io_context(|| format!("read time from {}", source.display()))?;
    fs::OpenOptions::new()
        .write(true)
        .open(copied)
        .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
        .io_context(|| format!("set time on {}", copied.display()))
}

/// Remove the merges a dry run staged for `workspace`.
pub fn discard_staging(workspace: &Workspace) {
    let journal = journal::path(OperationKind::Pull, &workspace.name, workspace.fork_time_ns);
    let _ = fs::remove_dir_all(journal::staging_dir(&journal));
}

/// Mark the pull journal at `path` done and remove it.
pub fn complete(path: &Path) -> Result {
    journal::set_phase(path, OperationPhase::Done)?;
    journal::remove(path)
}

/// The interrupted pull journal of `workspace`, if any.
pub fn find_open_journal(workspace: &str) -> Result<Option<PathBuf>> {
    journal::find_open(OperationKind::Pull, workspace)
}

/// Whether both entries have the same kind, permissions, and content (or are both missing).
pub(crate) fn equivalent(left: &Path, right: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::symlink_metadata(left), fs::symlink_metadata(right)) else {
        return fs::symlink_metadata(left).is_err() && fs::symlink_metadata(right).is_err();
    };
    if ManifestKind::of(&a) != ManifestKind::of(&b) || mode_bits(&a) != mode_bits(&b) {
        return false;
    }
    if a.file_type().is_symlink() {
        return fs::read_link(left).ok() == fs::read_link(right).ok();
    }
    a.is_dir() || (a.len() == b.len() && same_contents(left, right))
}
