//! `forkspace save` and `restore`: snapshots of a workspace to go back to.

use crate::cli::{RestoreArgs, SaveArgs};
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::sync::checkpoint::{self, Checkpoint};
use crate::sync::{install, join_paths};
use crate::workspace::guard;

/// `save [name] [note]`. A single word is the workspace when one has that name, else the note
/// (for the workspace you are in).
pub fn save(args: SaveArgs) -> Result {
    let (name, note) = match (args.first, args.second) {
        (Some(word), None) if !State::peek()?.workspaces.contains_key(&word) => (None, Some(word)),
        (first, second) => (first, second),
    };
    let name = crate::workspace::pick(name)?;
    let saved = State::with_state(|state| {
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;
        checkpoint::create(&workspace, note, false, false)
    })?;
    println!(
        "saved snapshot {} of '{name}' ({} paths, {}); go back with `forkspace restore {name} {}`",
        saved.id,
        saved.entries.len(),
        human_bytes(saved.bytes),
        saved.id
    );
    Ok(())
}

/// `restore [name] [id]`: without an id, list the snapshots. A single number is the id when
/// no workspace has that name.
pub fn restore(args: RestoreArgs) -> Result {
    let (name, id) = match (args.first, args.second) {
        (Some(word), None) if !State::peek()?.workspaces.contains_key(&word) => (None, Some(word)),
        (first, second) => (first, second),
    };
    let name = crate::workspace::pick(name)?;
    let Some(id) = id else {
        return list(&name);
    };
    let id: u64 = id.parse().map_err(|_| {
        ForkspaceError::InvalidArgument(format!(
            "'{id}' is not a snapshot number; `forkspace restore {name}` lists them"
        ))
    })?;
    crate::changes::manifest::settle_now(&name, true)?;
    State::with_state(|state| {
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;
        // A restore that was interrupted is finished first.
        if checkpoint::resume(&workspace)? {
            println!("finished the interrupted restore of '{name}'");
        }
        if workspace
            .process
            .as_ref()
            .is_some_and(|process| process.is_alive())
        {
            eprintln!("warning: a process is running in '{name}'; dev servers will reload");
        }
        let restored = checkpoint::restore(&workspace, id)?;
        print!(
            "restored '{name}' to snapshot {id} ({} paths changed)",
            restored.changed.len()
        );
        match restored.safety {
            Some(safety) => println!("; undo with `forkspace restore {name} {safety}`"),
            None => println!(),
        }
        if !restored.unrestorable.is_empty() {
            eprintln!(
                "warning: left as they are because forkspace does not know how they looked when the workspace was made: {}",
                join_paths(&restored.unrestorable)
            );
        }
        if install::touches_dependencies(&restored.changed) {
            if let Some(manager) = install::detect_package_manager(&workspace) {
                install::run_install(&workspace.view_path, manager)?;
                println!("ran package install in workspace");
            }
        }
        Ok(())
    })
}

fn list(name: &str) -> Result {
    let checkpoints = checkpoint::list(name)?;
    if checkpoints.is_empty() {
        println!("no snapshots of '{name}' yet; make one with `forkspace save`");
        return Ok(());
    }
    println!("ID  CREATED                    PATHS  SIZE      NOTE");
    for saved in &checkpoints {
        println!(
            "{:<3} {:<26} {:<6} {:<9} {}",
            saved.id,
            saved.created,
            saved.entries.len(),
            human_bytes(saved.bytes),
            label(saved)
        );
    }
    Ok(())
}

fn label(saved: &Checkpoint) -> String {
    let mut label = saved.label.clone().unwrap_or_default();
    if saved.automatic {
        label = format!("(auto) {label}");
    }
    label.trim().to_string()
}

fn human_bytes(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
    }
}
