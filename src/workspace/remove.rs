//! Removing a workspace: stop its processes, destroy its view, and forget it.

use crate::backend::{select_backend, DestroyMode};
use crate::changes::manifest;
use crate::error::{ForkspaceError, Result};
use crate::git;
use crate::process::members;
use crate::state::State;
use crate::workspace::{guard, Workspace};

/// What removing a workspace does about processes still using it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Processes {
    /// List them and refuse.
    Refuse,
    /// Stop the ones running in the workspace (`drop --kill`), then refuse if any remain.
    Kill,
}

/// Remove `name` from `state` after destroying its on-disk view, refusing while processes
/// other than its `forkspace run` dev server still use it.
///
/// The state record is only dropped once the backend cleanup succeeded, so a busy mount
/// (EBUSY) leaves a workspace that can be dropped again later.
pub fn remove(state: &mut State, name: &str) -> Result {
    remove_with(state, name, Processes::Refuse)
}

/// [`remove`], choosing what happens to processes still using the workspace.
pub fn remove_with(state: &mut State, name: &str, processes: Processes) -> Result {
    guard::ensure_idle(name)?;
    let workspace = state.require_workspace(name)?.clone();
    warn_if_unlanded(state, name);

    if let Some(process) = &workspace.process {
        process.terminate();
    }
    ensure_unused(&workspace, processes)?;

    let backend = select_backend(Some(workspace.backend_kind))?;
    remove_after_cleanup(state, name, || {
        crate::util::trace::time("destroy", || {
            backend.destroy(&workspace, DestroyMode::Normal)
        })
    })
}

/// Refuse while processes use `workspace`; with [`Processes::Kill`], stop the ones running in
/// it first. Processes that only have files open there are never killed.
fn ensure_unused(workspace: &Workspace, processes: Processes) -> Result {
    let mut found = members::find(workspace);
    if found.is_empty() {
        return Ok(());
    }
    if processes == Processes::Kill {
        members::stop(workspace, &found);
        found = members::find(workspace);
        if found.is_empty() {
            return Ok(());
        }
    }
    let remedy = if processes == Processes::Refuse && found.iter().any(|member| member.owned) {
        format!(
            "use `forkspace drop {} --kill` to stop them",
            workspace.name
        )
    } else {
        "close them (they only have files open there), then drop again".to_string()
    };
    Err(ForkspaceError::WorkspaceInUse {
        workspace: workspace.name.clone(),
        processes: found
            .iter()
            .map(members::Member::describe)
            .collect::<Vec<_>>()
            .join(", "),
        remedy,
    })
}

/// `name` and all of its descendants, deepest first, so children go before parents.
pub fn with_descendants(state: &State, name: &str) -> Vec<String> {
    let mut names = vec![name.to_string()];
    let mut stack = vec![name.to_string()];
    while let Some(current) = stack.pop() {
        for child in state.children_of(&current) {
            stack.push(child.name.clone());
            names.push(child.name.clone());
        }
    }
    state.sorted_deepest_first(&mut names);
    names
}

fn remove_after_cleanup(state: &mut State, name: &str, cleanup: impl FnOnce() -> Result) -> Result {
    cleanup()?;
    crate::process::systemd::stop_slice(name);
    manifest::remove(name)?;
    crate::sync::ancestor::remove(name)?;
    crate::sync::checkpoint::remove_all(name)?;
    state.remove_workspace(name)?;
    state.trash_unused_frozen_copies();
    Ok(())
}

/// Dropping a `--branch` workspace whose branch tip is not in the root base loses history.
fn warn_if_unlanded(state: &State, name: &str) {
    let Ok(workspace) = state.require_workspace(name) else {
        return;
    };
    let Some(branch) = &workspace.branch else {
        return;
    };
    let root_base = state
        .parent_chain(name)
        .first()
        .map_or_else(|| workspace.base.clone(), |root| root.base.clone());
    let reference = format!("refs/heads/{branch}");
    let Ok(tip) = git::rev_parse(&workspace.view_path, &reference) else {
        return;
    };
    let landed = git::rev_parse(&root_base, &reference)
        .is_ok_and(|base_tip| git::is_ancestor(&root_base, &tip, &base_tip).unwrap_or(false));
    if !landed {
        eprintln!(
            "warning: workspace '{name}' has unlanded work on '{branch}' — run `forkspace land {name}` first, or accept history loss"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::remove_after_cleanup;
    use crate::error::ForkspaceError;
    use crate::state::State;
    use crate::workspace::Workspace;

    #[test]
    fn ebusy_cleanup_retains_workspace_state() {
        let mut state = State::default();
        state
            .add_workspace(Workspace::new("busy", "/tmp/base".into(), None))
            .unwrap();

        let result = remove_after_cleanup(&mut state, "busy", || {
            Err(ForkspaceError::Io {
                context: "remove busy workspace".to_string(),
                source: std::io::Error::from_raw_os_error(libc::EBUSY),
            })
        });

        assert!(result.is_err());
        assert!(state.workspaces.contains_key("busy"));
    }
}
