//! `forkspace gc`: show what disk forkspace could give back, and reclaim it after confirmation.
//!
//! Candidates are workspaces with no changes, no processes, and nothing else worth keeping
//! that are older than `--older-than`; frozen copies no workspace uses; undo data and
//! checkpoints past their retention; and leftovers (trash, fork-time copies of dropped
//! workspaces, staged merges). Workspaces with changes are listed with their size, never
//! removed.

use std::fs;
use std::io::{self, IsTerminal, Write as _};
use std::path::PathBuf;

use time::OffsetDateTime;

use crate::backend::{select_backend, ChangeCount};
use crate::cli::GcArgs;
use crate::disk;
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::process::{members, systemd};
use crate::state::State;
use crate::sync::{checkpoint, merge, undo};
use crate::workspace::{self, Workspace};

/// One thing `gc` would remove.
struct Candidate {
    what: String,
    why: String,
    bytes: u64,
    target: Target,
}

enum Target {
    Workspace(String),
    /// A directory, removed only if `Recheck` still holds under the state lock: the survey ran
    /// without it, and a command may have started using the directory since (GC-09).
    Directory(PathBuf, Recheck),
    Slice(String),
}

/// Why a directory could go, checked again right before it is removed.
#[derive(Clone)]
enum Recheck {
    /// A frozen copy no workspace uses.
    UnusedFrozenCopy,
    /// Undo data past retention.
    ExpiredUndo,
    /// A checkpoint of workspace `name` past retention.
    ExpiredCheckpoint(String),
    /// Trash: nothing ever uses it again.
    Trash,
    /// A fork-time copy or checkpoints of workspace `name`, which no workspace has.
    Leftover(String),
    /// A merge's staging directory whose journal is gone.
    StagedMerge,
}

impl Recheck {
    /// Whether `path` may still be removed, judged on `state` (the state lock is held).
    fn holds(&self, state: &State, path: &std::path::Path) -> bool {
        match self {
            Self::UnusedFrozenCopy => state.unused_frozen_copies().iter().any(|copy| copy == path),
            Self::ExpiredUndo => undo::expired().iter().any(|expired| expired == path),
            Self::ExpiredCheckpoint(name) => checkpoint::expired(name)
                .iter()
                .any(|expired| expired == path),
            Self::Trash => true,
            Self::Leftover(name) => !state.name_in_use(name),
            Self::StagedMerge => !path.with_extension("json").exists(),
        }
    }
}

pub fn exec(args: GcArgs) -> Result {
    let older_than = parse_age(&args.older_than)?;
    // Survey without the state lock: the confirmation prompt can take a while, and agents'
    // commands must not wait on it.
    let Survey { candidates, kept } = survey(&State::load()?, older_than)?;
    for (name, bytes) in &kept {
        println!("keeping '{name}': it has changes ({})", disk::human(*bytes));
    }
    if candidates.is_empty() {
        println!("nothing to clean up");
        return Ok(());
    }
    let total: u64 = candidates.iter().map(|candidate| candidate.bytes).sum();
    println!("forkspace gc can remove:");
    let width = candidates
        .iter()
        .map(|candidate| candidate.what.len())
        .max()
        .unwrap_or(0);
    for candidate in &candidates {
        println!(
            "  {:<width$}  {:>8}  {}",
            candidate.what,
            disk::human(candidate.bytes),
            candidate.why
        );
    }
    println!("total: {}", disk::human(total));
    if !confirmed(args.yes) {
        println!("nothing removed; run `forkspace gc --yes` to remove them");
        return Ok(());
    }
    let freed = State::with_state(|state| {
        let mut freed = 0;
        for candidate in candidates {
            if remove(state, &candidate.target, older_than) {
                freed += candidate.bytes;
            }
        }
        // Private TMPDIRs of workspaces that no longer exist (a crash before `drop` ended).
        let names: Vec<&str> = state.workspaces.keys().map(String::as_str).collect();
        crate::workspace::tmp::sweep(&names);
        Ok(freed)
    })?;
    println!("freed {}", disk::human(freed));
    Ok(())
}

/// Remove one candidate; a workspace is checked again first, since it may have changed
/// while the user was deciding.
fn remove(state: &mut State, target: &Target, older_than: u64) -> bool {
    match target {
        Target::Workspace(name) => {
            let still_idle = state.workspaces.get(name).is_some_and(|workspace| {
                !has_changes(workspace).unwrap_or(true)
                    && idle(state, workspace)
                    && age_of(workspace) >= older_than
            });
            if !still_idle {
                eprintln!("warning: kept '{name}': it changed since the survey");
                return false;
            }
            workspace::remove(state, name)
                .map_err(|error| eprintln!("warning: kept '{name}': {error}"))
                .is_ok()
        }
        Target::Directory(path, recheck) => {
            if !recheck.holds(state, path) {
                eprintln!(
                    "warning: kept {}: it is in use since the survey",
                    path.display()
                );
                return false;
            }
            fs::remove_dir_all(path)
                .map_err(|error| eprintln!("warning: could not remove {}: {error}", path.display()))
                .is_ok()
        }
        Target::Slice(unit) => {
            systemd::stop_unit(unit);
            true
        }
    }
}

/// Seconds since `workspace` was created.
fn age_of(workspace: &Workspace) -> u64 {
    OffsetDateTime::parse(
        &workspace.created,
        &time::format_description::well_known::Rfc3339,
    )
    .map(|created| (OffsetDateTime::now_utc() - created).whole_seconds().max(0) as u64)
    .unwrap_or(0)
}

/// What `gc` found.
struct Survey {
    candidates: Vec<Candidate>,
    /// Workspaces kept because they have changes, with their size.
    kept: Vec<(String, u64)>,
}

/// What can go, and the workspaces kept because they have changes.
fn survey(state: &State, older_than: u64) -> Result<Survey> {
    let mut candidates = Vec::new();
    let mut kept = Vec::new();
    let mut names: Vec<&String> = state.workspaces.keys().collect();
    names.sort();
    for name in names {
        let workspace = &state.workspaces[name];
        if !workspace.view_path.is_dir() {
            // Deleted or moved by hand (STATE-12): whatever it held is out of forkspace's reach,
            // and only the user can say whether the record is still wanted.
            println!(
                "keeping '{name}': its folder {} is missing; `forkspace drop {name}` removes its record",
                workspace.view_path.display()
            );
            continue;
        }
        let bytes = disk::workspace_bytes(workspace).unwrap_or(0);
        match has_changes(workspace) {
            Ok(false) => {}
            Ok(true) => {
                kept.push((name.clone(), bytes));
                continue;
            }
            Err(error) => {
                eprintln!("warning: keeping '{name}': cannot tell whether it has changes: {error}");
                continue;
            }
        }
        let age = age_of(workspace);
        if age < older_than || !idle(state, workspace) {
            continue;
        }
        candidates.push(Candidate {
            what: format!("workspace '{name}'"),
            why: format!("no changes, no processes, {} old", describe_age(age)),
            bytes,
            target: Target::Workspace(name.clone()),
        });
    }

    for path in state.unused_frozen_copies() {
        candidates.push(directory(
            path,
            "frozen copy",
            "no workspace uses it",
            Recheck::UnusedFrozenCopy,
        ));
    }
    for path in undo::expired() {
        candidates.push(directory(
            path,
            "undo data",
            "past retention (10 merges or 14 days)",
            Recheck::ExpiredUndo,
        ));
    }
    for name in state.workspaces.keys() {
        for path in checkpoint::expired(name) {
            candidates.push(directory(
                path,
                "checkpoint",
                "past retention",
                Recheck::ExpiredCheckpoint(name.clone()),
            ));
        }
    }
    for path in entries(&paths::trash_dir()) {
        candidates.push(directory(
            path,
            "trash",
            "left by earlier drops",
            Recheck::Trash,
        ));
    }
    // The workspace loop above can take a while: judge leftovers by the state as it is now.
    let current = State::peek()?;
    for root in [paths::ancestor_root(), paths::checkpoint_root()] {
        for path in entries(&root) {
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let owner = file_name
                .trim_start_matches('.')
                .split(".tmp-")
                .next()
                .unwrap_or(file_name)
                .to_string();
            if !current.name_in_use(&owner) {
                candidates.push(directory(
                    path,
                    "leftover",
                    "belongs to no workspace",
                    Recheck::Leftover(owner),
                ));
            }
        }
    }
    for path in entries(&paths::journal_dir()) {
        let orphan = path.extension().is_some_and(|extension| extension == "d")
            && !path.with_extension("json").exists();
        if orphan {
            candidates.push(directory(
                path,
                "staged merge",
                "its operation is gone",
                Recheck::StagedMerge,
            ));
        }
    }
    for unit in systemd::orphan_slices(state.workspaces.keys()) {
        candidates.push(Candidate {
            what: format!("slice {unit}"),
            why: "belongs to no workspace".to_string(),
            bytes: 0,
            target: Target::Slice(unit),
        });
    }
    Ok(Survey { candidates, kept })
}

/// Whether removing `workspace` could lose work: changed files, commits, or saved
/// checkpoints.
fn has_changes(workspace: &Workspace) -> Result<bool> {
    let backend = select_backend(Some(workspace.backend_kind))?;
    let unchanged = matches!(
        backend.change_count(workspace, true)?,
        ChangeCount::Ready(0)
    );
    let manual_checkpoints = checkpoint::list(&workspace.name)?
        .iter()
        .any(|saved| !saved.automatic);
    Ok(!unchanged
        || manual_checkpoints
        || merge::workspace_head_changed(workspace).unwrap_or(true)
        || !workspace.unresolved_conflicts.is_empty())
}

/// No processes, no children, and no operation in flight.
fn idle(state: &State, workspace: &Workspace) -> bool {
    state.children_of(&workspace.name).is_empty()
        && !workspace
            .process
            .as_ref()
            .is_some_and(|process| process.is_alive())
        && members::find(workspace).is_empty()
        // `find` leaves out gc's own process and its ancestors (so `drop` works from inside a
        // workspace); a shell that runs gc from inside one still uses it (GC-10).
        && !members::used_by_own_lineage(workspace)
        && workspace::guard::ensure_idle(&workspace.name).is_ok()
}

fn directory(path: PathBuf, what: &str, why: &str, recheck: Recheck) -> Candidate {
    Candidate {
        what: format!(
            "{what} {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        why: why.to_string(),
        bytes: disk::tree_private_bytes(&path).unwrap_or(0),
        target: Target::Directory(path, recheck),
    }
}

/// Entries of `directory`, without the backup exclusion tag.
fn entries(directory: &std::path::Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

/// `--yes`, or a `y` typed at a terminal.
fn confirmed(yes: bool) -> bool {
    if yes {
        return true;
    }
    if !io::stdin().is_terminal() {
        return false;
    }
    eprint!("remove them? [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes")
}

/// `7d`, `12h`, `30m`, or plain seconds.
fn parse_age(value: &str) -> Result<u64> {
    let (number, unit) = value.split_at(value.trim_end_matches(char::is_alphabetic).len());
    let number: u64 = number.parse().map_err(|_| invalid_age(value))?;
    let seconds = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(invalid_age(value)),
    };
    Ok(number * seconds)
}

fn invalid_age(value: &str) -> ForkspaceError {
    ForkspaceError::InvalidArgument(format!(
        "invalid age '{value}'; use a number with s, m, h, d, or w (for example 7d)"
    ))
}

fn describe_age(seconds: u64) -> String {
    match seconds {
        0..3600 => format!("{} minutes", seconds / 60),
        3600..86_400 => format!("{} hours", seconds / 3600),
        _ => format!("{} days", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GC-09: a leftover found by the survey is removed only if no workspace has its name by
    /// then: one created meanwhile keeps its fork-time copy and checkpoints.
    #[test]
    fn a_leftover_is_kept_when_its_workspace_appeared_since_the_survey() {
        let root = tempfile::tempdir().unwrap();
        let leftover = root.path().join("victim");
        fs::create_dir(&leftover).unwrap();
        let target = Target::Directory(leftover.clone(), Recheck::Leftover("victim".to_string()));

        let mut state = State::default();
        state
            .add_workspace(Workspace::new("victim", root.path().to_path_buf(), None))
            .unwrap();
        assert!(!remove(&mut state, &target, 0));
        assert!(leftover.is_dir());

        let mut state = State::default();
        assert!(remove(&mut state, &target, 0));
        assert!(!leftover.exists());
    }
}
