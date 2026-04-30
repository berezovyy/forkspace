//! Removing a workspace: stop its process, destroy its view, and forget it.

use crate::backend::{select_backend, DestroyMode};
use crate::changes::manifest;
use crate::error::Result;
use crate::git;
use crate::state::State;
use crate::workspace::guard;

/// Remove `name` from `state` after destroying its on-disk view.
///
/// The state record is only dropped once the backend cleanup succeeded, so a busy mount
/// (EBUSY) leaves a workspace that can be dropped again later.
pub fn remove(state: &mut State, name: &str) -> Result {
    guard::ensure_idle(name)?;
    let workspace = state.require_workspace(name)?.clone();
    warn_if_unlanded(state, name);

    if let Some(process) = &workspace.process {
        process.terminate();
    }

    let backend = select_backend(Some(workspace.backend_kind))?;
    remove_after_cleanup(state, name, || {
        backend.destroy(&workspace, DestroyMode::Normal)
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
    manifest::remove(name)?;
    state.remove_workspace(name)
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
