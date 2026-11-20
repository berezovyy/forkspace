//! Which workspace a command means when it is not named: the one whose folder you are in, as
//! Git knows the repository from the folder, or else the only one there is.

use std::path::Path;

use crate::error::{ForkspaceError, Result};
use crate::state::State;

/// The workspace `name` names, or the one the current folder is in, or the only workspace.
pub fn pick(name: Option<String>) -> Result<String> {
    if let Some(name) = name {
        return Ok(name);
    }
    let state = State::peek()?;
    let here = std::env::current_dir()
        .ok()
        .and_then(|directory| directory.canonicalize().ok());
    if let Some(here) = &here {
        if let Some(name) = containing(&state, here) {
            return Ok(name);
        }
    }
    let mut names: Vec<&String> = state.workspaces.keys().collect();
    match names.len() {
        0 => Err(ForkspaceError::InvalidArgument(
            "there are no workspaces yet; make one with `forkspace new`".to_string(),
        )),
        1 => Ok(names.remove(0).clone()),
        _ => {
            names.sort();
            Err(ForkspaceError::InvalidArgument(format!(
                "which workspace? name one of: {}",
                names
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    }
}

/// The workspace whose folder holds `directory` (the deepest one, for a workspace inside
/// another's folder).
fn containing(state: &State, directory: &Path) -> Option<String> {
    state
        .workspaces
        .values()
        .filter_map(|workspace| {
            let view = workspace.view_path.canonicalize().ok()?;
            directory
                .starts_with(&view)
                .then(|| (view.components().count(), workspace.name.clone()))
        })
        .max()
        .map(|(_, name)| name)
}
