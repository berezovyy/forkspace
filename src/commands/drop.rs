use std::fs;

use crate::error::{ForkspaceError, Result};
use crate::overlay::{unmount_overlay, unmount_tmpfs};
use crate::state::State;
use crate::util::{glob_matches, forkspace_home, is_pid_alive, kill_process};

pub(crate) fn remove_workspace(state: &mut State, name: &str) -> Result {
    let ws = state.require_workspace(name)?.clone();

    if let Some(ref proc) = ws.process {
        if is_pid_alive(proc.pid) {
            kill_process(proc.pid);
        }
    }

    unmount_overlay(&ws.merged)?;

    if ws.tmpfs {
        if let Err(e) = unmount_tmpfs(&ws.upper) {
            eprintln!("warning: failed to unmount tmpfs upper for '{}': {e}", name);
        }
        if let Err(e) = unmount_tmpfs(&ws.work) {
            eprintln!("warning: failed to unmount tmpfs work for '{}': {e}", name);
        }
    }

    let ws_dir = forkspace_home().join(name);
    remove_workspace_record_after_cleanup(state, name, || fs::remove_dir_all(&ws_dir))?;

    Ok(())
}

fn remove_workspace_record_after_cleanup<F>(state: &mut State, name: &str, cleanup: F) -> Result
where
    F: FnOnce() -> std::io::Result<()>,
{
    cleanup().map_err(|e| ForkspaceError::Io {
        context: format!(
            "remove workspace directory {}; retry `forkspace drop {name}` after closing processes using it",
            forkspace_home().join(name).display()
        ),
        source: e,
    })?;
    state.remove_workspace(name)
}

fn collect_descendants(state: &State, name: &str) -> Vec<String> {
    let mut descendants = Vec::new();
    let mut stack = vec![name.to_string()];

    while let Some(current) = stack.pop() {
        let children: Vec<String> = state
            .children_of(&current)
            .iter()
            .map(|ws| ws.name.clone())
            .collect();
        for child in children {
            stack.push(child.clone());
            descendants.push(child);
        }
    }

    descendants
}

pub fn exec(args: crate::cli::DropArgs) -> Result {
    if args.all {
        return run_drop_all();
    }

    let name = args.name.ok_or_else(|| {
        ForkspaceError::InvalidArgument(
            "workspace name is required unless --all is specified".to_string(),
        )
    })?;

    if args.glob {
        return run_drop_glob(&name);
    }

    State::with_state(|state| {
        state.require_workspace(&name)?;

        let children: Vec<String> = state
            .children_of(&name)
            .iter()
            .map(|ws| ws.name.clone())
            .collect();

        if !children.is_empty() && !args.force {
            return Err(ForkspaceError::HasChildren {
                workspace: name.clone(),
                children,
            });
        }

        let mut to_drop = Vec::new();
        if args.force {
            let descendants = collect_descendants(state, &name);
            to_drop.extend(descendants);
        }
        to_drop.push(name.clone());

        state.sorted_deepest_first(&mut to_drop);

        let count = to_drop.len();
        for ws_name in &to_drop {
            remove_workspace(state, ws_name)?;
        }

        if count == 1 {
            println!("removed '{}'", name);
        } else {
            println!("removed {} workspaces (including '{}')", count, name);
        }

        Ok(())
    })
}

fn run_drop_all() -> Result {
    State::with_state(|state| {
        let mut names = state.workspace_names();

        if names.is_empty() {
            println!("no workspaces to remove");
            return Ok(());
        }

        state.sorted_deepest_first(&mut names);

        let count = names.len();
        for ws_name in &names {
            remove_workspace(state, ws_name)?;
        }

        println!("removed all {} workspaces", count);
        Ok(())
    })
}

fn run_drop_glob(pattern: &str) -> Result {
    State::with_state(|state| {
        let mut matches: Vec<String> = state
            .workspaces
            .values()
            .filter(|ws| glob_matches(pattern, &ws.name))
            .map(|ws| ws.name.clone())
            .collect();

        if matches.is_empty() {
            println!("no workspaces match '{}'", pattern);
            return Ok(());
        }

        state.sorted_deepest_first(&mut matches);

        let count = matches.len();
        for ws_name in &matches {
            remove_workspace(state, ws_name)?;
        }

        println!("removed {} workspaces matching '{}'", count, pattern);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::remove_workspace_record_after_cleanup;
    use crate::state::State;
    use crate::workspace::Workspace;

    #[test]
    fn ebusy_cleanup_retains_workspace_state() {
        let mut state = State::default();
        state
            .add_workspace(Workspace::new("busy", "/tmp/base".into(), None))
            .unwrap();

        let result = remove_workspace_record_after_cleanup(&mut state, "busy", || {
            Err(std::io::Error::from_raw_os_error(libc::EBUSY))
        });

        assert!(result.is_err());
        assert!(state.workspaces.contains_key("busy"));
    }
}
