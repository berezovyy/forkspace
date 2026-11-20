//! `forkspace drop`: throw workspaces away, stopping whatever still runs in them.

use crate::cli::DropArgs;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::workspace;
use crate::workspace::remove::Processes;

pub fn exec(args: DropArgs) -> Result {
    State::with_state(|state| {
        let mut names = args.names.clone();
        names.sort();
        names.dedup();
        for name in &names {
            state.require_workspace(name)?;
        }
        // A workspace others were made from goes only together with them.
        for name in &names {
            let children: Vec<String> = state
                .children_of(name)
                .iter()
                .map(|child| child.name.clone())
                .filter(|child| !names.contains(child))
                .collect();
            if !children.is_empty() {
                return Err(ForkspaceError::HasChildren {
                    workspace: name.clone(),
                    children,
                });
            }
        }
        state.sorted_deepest_first(&mut names);
        let failed = remove_all(state, &names)?;
        if failed.is_none() {
            for name in &names {
                println!("removed '{name}'");
            }
        }
        Ok(failed)
    })
    .and_then(fail)
}

/// Remove `names` (ordered deepest first), stopping their processes. If one removal fails,
/// the ones before it stay removed (the caller saves state) and the failure is returned, so a
/// destroyed view is never left recorded.
fn remove_all(state: &mut State, names: &[String]) -> Result<Option<ForkspaceError>> {
    crate::process::systemd::batch_slice_stops(|| remove_each(state, names))
}

fn remove_each(state: &mut State, names: &[String]) -> Result<Option<ForkspaceError>> {
    let recorded = names
        .iter()
        .map(|name| state.require_workspace(name))
        .collect::<Result<Vec<_>>>()?;
    // One scan of the machine's processes tells which workspaces have nothing to stop and need
    // no scan of their own.
    let found = crate::process::members::find_all(&recorded);
    let idle: Vec<String> = recorded
        .iter()
        .filter(|workspace| found.get(&workspace.name).is_none_or(Vec::is_empty))
        .map(|workspace| workspace.name.clone())
        .collect();
    for (index, name) in names.iter().enumerate() {
        let removed = if idle.contains(name) {
            workspace::remove::remove_checked(state, name, Processes::Kill)
        } else {
            workspace::remove::remove_with(state, name, Processes::Kill)
        };
        if let Err(error) = removed {
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
