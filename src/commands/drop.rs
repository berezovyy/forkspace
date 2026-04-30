//! `forkspace drop`: remove one workspace (with `--force`, its descendants too), every workspace
//! matching a glob, or all of them.

use crate::cli::DropArgs;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::util::glob_matches;
use crate::workspace;

pub fn exec(args: DropArgs) -> Result {
    if args.all {
        return drop_all();
    }
    let name = args.name.ok_or_else(|| {
        ForkspaceError::InvalidArgument(
            "workspace name is required unless --all is specified".to_string(),
        )
    })?;
    if args.glob {
        drop_matching(&name)
    } else {
        drop_one(&name, args.force)
    }
}

/// Drop `name`; its descendants go too with `force`, otherwise their presence is an error.
fn drop_one(name: &str, force: bool) -> Result {
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
        remove_all(state, &names)?;
        if names.len() == 1 {
            println!("removed '{name}'");
        } else {
            println!("removed {} workspaces (including '{name}')", names.len());
        }
        Ok(())
    })
}

fn drop_all() -> Result {
    State::with_state(|state| {
        let mut names = state.workspace_names();
        if names.is_empty() {
            println!("no workspaces to remove");
            return Ok(());
        }
        state.sorted_deepest_first(&mut names);
        remove_all(state, &names)?;
        println!("removed all {} workspaces", names.len());
        Ok(())
    })
}

fn drop_matching(pattern: &str) -> Result {
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
        remove_all(state, &names)?;
        println!("removed {} workspaces matching '{pattern}'", names.len());
        Ok(())
    })
}

/// `names` must already be ordered deepest first.
fn remove_all(state: &mut State, names: &[String]) -> Result {
    names
        .iter()
        .try_for_each(|name| workspace::remove(state, name))
}
