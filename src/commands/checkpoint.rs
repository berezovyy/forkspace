//! `forkspace checkpoint`, `checkpoints`, and `restore`: save points inside a workspace.

use crate::cli::{CheckpointArgs, CheckpointsArgs, RestoreArgs};
use crate::error::Result;
use crate::state::State;
use crate::sync::checkpoint::{self, Checkpoint};
use crate::sync::three_way::is_lockfile;
use crate::sync::{install, join_paths};
use crate::workspace::guard;

use super::pull::workspace_name;

pub fn save(args: CheckpointArgs) -> Result {
    let name = workspace_name(args.name)?;
    let saved = State::with_state(|state| {
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;
        checkpoint::create(&workspace, args.message, args.all, false)
    })?;
    println!(
        "saved checkpoint {} of '{name}' ({} paths, {})",
        saved.id,
        saved.entries.len(),
        human_bytes(saved.bytes)
    );
    Ok(())
}

pub fn list(args: CheckpointsArgs) -> Result {
    let name = workspace_name(args.name)?;
    State::load()?.require_workspace(&name)?;
    let checkpoints = checkpoint::list(&name)?;
    if args.json {
        return super::print_json(&checkpoints);
    }
    if checkpoints.is_empty() {
        println!("no checkpoints in '{name}'");
        return Ok(());
    }
    println!("ID  CREATED                    PATHS  SIZE      LABEL");
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

pub fn restore(args: RestoreArgs) -> Result {
    let name = args.name;
    State::with_state(|state| {
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;
        if args.resume {
            if checkpoint::resume(&workspace)? {
                println!("finished the interrupted restore of '{name}'");
            } else {
                println!("no interrupted restore in '{name}'");
            }
            return Ok(());
        }
        let id = args.id.unwrap_or_default();
        if workspace
            .process
            .as_ref()
            .is_some_and(|process| process.is_alive())
        {
            eprintln!("warning: a process is running in '{name}'; dev servers will reload");
        }
        let restored = checkpoint::restore(&workspace, id)?;
        print!(
            "restored '{name}' to checkpoint {id} ({} paths changed)",
            restored.changed.len()
        );
        match restored.safety {
            Some(safety) => println!("; undo with `forkspace restore {name} {safety}`"),
            None => println!(),
        }
        if !restored.unrestorable.is_empty() {
            eprintln!(
                "warning: left as they are because their fork-time version is unknown: {}",
                join_paths(&restored.unrestorable)
            );
        }
        let lockfile_changed = restored.changed.iter().any(|path| is_lockfile(path));
        if lockfile_changed && !args.no_install {
            if let Some(manager) = install::detect_package_manager(&workspace) {
                install::run_install(&workspace.view_path, manager)?;
                println!("ran package install in workspace");
            }
        }
        Ok(())
    })
}

fn label(saved: &Checkpoint) -> String {
    let mut label = saved.label.clone().unwrap_or_default();
    if saved.automatic {
        label = format!("(auto) {label}");
    }
    if saved.all {
        label.push_str(" [all]");
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
