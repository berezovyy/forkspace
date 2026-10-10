//! `forkspace drop`: remove one workspace (with `--force`, its descendants too), every workspace
//! matching a glob, or all of them.

use crate::cli::DropArgs;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::util::glob_matches;
use crate::workspace;
use crate::workspace::remove::Processes;

pub fn exec(args: DropArgs) -> Result {
    let processes = if args.kill {
        Processes::Kill
    } else {
        Processes::Refuse
    };
    if args.all {
        return drop_all(processes);
    }
    let name = args.name.ok_or_else(|| {
        ForkspaceError::InvalidArgument(
            "workspace name is required unless --all is specified".to_string(),
        )
    })?;
    if args.glob {
        drop_matching(&name, processes)
    } else {
        drop_one(&name, args.force, processes)
    }
}

/// Drop `name`; its descendants go too with `force`, otherwise their presence is an error.
fn drop_one(name: &str, force: bool, processes: Processes) -> Result {
    State::with_state(|state| {
        state.require_workspace(name)?;
        let children: Vec<String> = state
            .children_of(name)
            .iter()
            .map(|ws| ws.name.clone())
            .collect();
        if !children.is_empty() && !force {
            return Err(ForkspaceError::HasChildren {
                workspace: name.to_string(),
                children,
            });
        }

        let names = workspace::remove::with_descendants(state, name);
        let failed = remove_all(state, &names, processes)?;
        if failed.is_none() {
            if names.len() == 1 {
                println!("removed '{name}'");
            } else {
                println!("removed {} workspaces (including '{name}')", names.len());
            }
        }
        Ok(failed)
    })
    .and_then(fail)
}

fn drop_all(processes: Processes) -> Result {
    State::with_state(|state| {
        let mut names = state.workspace_names();
        if names.is_empty() {
            println!("no workspaces to remove");
            return Ok(None);
        }
        state.sorted_deepest_first(&mut names);
        let failed = remove_all(state, &names, processes)?;
        if failed.is_none() {
            println!("removed all {} workspaces", names.len());
        }
        Ok(failed)
    })
    .and_then(fail)
}

fn drop_matching(pattern: &str, processes: Processes) -> Result {
    State::with_state(|state| {
        let mut names: Vec<String> = state
            .workspaces
            .keys()
            .filter(|name| glob_matches(pattern, name))
            .cloned()
            .collect();
        if names.is_empty() {
            println!("no workspaces match '{pattern}'");
            return Ok(None);
        }
        state.sorted_deepest_first(&mut names);
        let failed = remove_all(state, &names, processes)?;
        if failed.is_none() {
            println!("removed {} workspaces matching '{pattern}'", names.len());
        }
        Ok(failed)
    })
    .and_then(fail)
}

/// Remove `names` (ordered deepest first). Processes are checked for every workspace before
/// any is destroyed. If one removal fails, the ones before it stay removed (the caller saves
/// state) and the failure is returned, so a destroyed view is never left recorded.
fn remove_all(
    state: &mut State,
    names: &[String],
    processes: Processes,
) -> Result<Option<ForkspaceError>> {
    if processes == Processes::Refuse {
        for name in names {
            let recorded = state.require_workspace(name)?.clone();
            workspace::remove::ensure_unused(&recorded, processes)?;
        }
    }
    for (index, name) in names.iter().enumerate() {
        if let Err(error) = workspace::remove::remove_with(state, name, processes) {
            if index > 0 {
                eprintln!("removed {} workspaces before '{name}' failed", index);
            }
            return Ok(Some(error));
        }
    }
    Ok(None)
}

/// Turn a recorded removal failure into the command's error, after state was saved.
fn fail(failed: Option<ForkspaceError>) -> Result {
    failed.map_or(Ok(()), Err)
}
