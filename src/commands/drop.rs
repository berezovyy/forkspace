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
        remove_all(state, &names, processes)?;
        if names.len() == 1 {
            println!("removed '{name}'");
        } else {
            println!("removed {} workspaces (including '{name}')", names.len());
        }
        Ok(())
    })
}

fn drop_all(processes: Processes) -> Result {
    State::with_state(|state| {
        let mut names = state.workspace_names();
        if names.is_empty() {
            println!("no workspaces to remove");
            return Ok(());
        }
        state.sorted_deepest_first(&mut names);
        remove_all(state, &names, processes)?;
        println!("removed all {} workspaces", names.len());
        Ok(())
    })
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
            return Ok(());
        }
        state.sorted_deepest_first(&mut names);
        remove_all(state, &names, processes)?;
        println!("removed {} workspaces matching '{pattern}'", names.len());
        Ok(())
    })
}

/// `names` must already be ordered deepest first.
fn remove_all(state: &mut State, names: &[String], processes: Processes) -> Result {
    names
        .iter()
        .try_for_each(|name| workspace::remove::remove_with(state, name, processes))
}
