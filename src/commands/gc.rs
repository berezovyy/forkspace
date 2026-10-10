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
    Directory(PathBuf),
    Slice(String),
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
        Target::Directory(path) => fs::remove_dir_all(path)
            .map_err(|error| eprintln!("warning: could not remove {}: {error}", path.display()))
            .is_ok(),
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
        let bytes = disk::workspace_bytes(workspace).unwrap_or(0);
        if has_changes(workspace)? {
            kept.push((name.clone(), bytes));
            continue;
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
        candidates.push(directory(path, "frozen copy", "no workspace uses it"));
    }
    for path in undo::expired() {
        candidates.push(directory(
            path,
            "undo data",
            "past retention (10 merges or 14 days)",
        ));
    }
    for name in state.workspaces.keys() {
        for path in checkpoint::expired(name) {
            candidates.push(directory(path, "checkpoint", "past retention"));
        }
    }
    for path in entries(&paths::trash_dir()) {
        candidates.push(directory(path, "trash", "left by earlier drops"));
    }
    for root in [paths::ancestor_root(), paths::checkpoint_root()] {
        for path in entries(&root) {
            let owner = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !state.name_in_use(
                owner
                    .trim_start_matches('.')
                    .split(".tmp-")
                    .next()
                    .unwrap_or(owner),
            ) {
                candidates.push(directory(path, "leftover", "belongs to no workspace"));
            }
        }
    }
    for path in entries(&paths::journal_dir()) {
        let orphan = path.extension().is_some_and(|extension| extension == "d")
            && !path.with_extension("json").exists();
        if orphan {
            candidates.push(directory(path, "staged merge", "its operation is gone"));
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
        && workspace::guard::ensure_idle(&workspace.name).is_ok()
}

fn directory(path: PathBuf, what: &str, why: &str) -> Candidate {
    Candidate {
        what: format!(
            "{what} {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        why: why.to_string(),
        bytes: disk::tree_private_bytes(&path).unwrap_or(0),
        target: Target::Directory(path),
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
