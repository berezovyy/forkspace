//! Creating a workspace: resolve the origin, pick a backend, and materialize the view.
//!
//! Creation is recorded as a pending intent in state before any files are written, so a
//! crash mid-materialization leaves a record that recovery can clean up.

use std::path::PathBuf;

use crate::backend::{
    pending_create_path, select_backend, select_backend_for_create, CreateReq, WorkspaceBackend,
};
use crate::error::{ForkspaceError, Result};
use crate::process::watch;
use crate::state::{PendingCreate, State};
use crate::util::IoContext;
use crate::workspace::{guard, Workspace};

/// Stacks deeper than this are refused; merge or fork from an earlier ancestor instead.
const MAX_FORK_DEPTH: usize = 32;

#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub session: Option<String>,
    /// Reserved; every backend currently rejects it.
    pub tmpfs: bool,
    pub tmpfs_size: Option<String>,
    /// Skip capability probing and use the portable full-copy backend.
    pub force_copy: bool,
}

/// Where a new workspace's contents come from, and the backend that will copy them.
struct Origin {
    path: PathBuf,
    parent: Option<String>,
    backend: Box<dyn WorkspaceBackend>,
}

/// Fork `base` into a new workspace called `name`.
///
/// If `base` is the name of an existing workspace, the new workspace is stacked on top of it
/// and inherits its backend (unless `force_copy` is set).
pub fn create(base: PathBuf, name: &str, options: CreateOptions) -> Result<Workspace> {
    validate_name(name)?;
    let Origin {
        path: origin,
        parent,
        backend,
    } = resolve_origin(base, name, options.force_copy)?;

    backend.preflight(&origin)?;
    let origin = origin
        .canonicalize()
        .io_context(|| format!("failed to canonicalize base path: {}", origin.display()))?;

    record_intent(name, backend.as_ref())?;
    let request = CreateReq {
        name: name.to_string(),
        base: origin.clone(),
        tmpfs: options.tmpfs,
        tmpfs_size: options.tmpfs_size,
    };
    let view = match backend.create(&request) {
        Ok(view) => view,
        Err(error) => {
            State::with_state(|state| {
                clear_intent(state, name);
                Ok(())
            })?;
            return Err(error);
        }
    };

    let workspace = Workspace::from_view(
        name.to_string(),
        origin,
        parent,
        options.session,
        backend.kind(),
        view,
    );
    State::with_state(|state| {
        clear_intent(state, name);
        state.add_workspace(workspace.clone())
    })?;
    Ok(workspace)
}

/// Resolve `base` to a directory, stacking on the workspace of that name if there is one.
fn resolve_origin(base: PathBuf, name: &str, force_copy: bool) -> Result<Origin> {
    let state = State::load()?;
    ensure_name_free(&state, name)?;

    let Some(parent) = state.workspaces.get(base.to_str().unwrap_or("")) else {
        let backend = select_backend_for_create(&base, force_copy)?;
        return Ok(Origin {
            path: base,
            parent: None,
            backend,
        });
    };

    guard::ensure_idle(&parent.name)?;
    if state.depth_of(&parent.name) + 1 >= MAX_FORK_DEPTH {
        return Err(ForkspaceError::InvalidArgument(format!(
            "workspace stack depth is limited to {MAX_FORK_DEPTH}; merge or fork from an earlier ancestor"
        )));
    }
    guard::require_live_view(&state, parent)?;
    let path = select_backend(Some(parent.backend_kind))?.view_path(parent);
    let backend = if force_copy {
        select_backend_for_create(&path, true)?
    } else {
        select_backend(Some(parent.backend_kind))?
    };
    Ok(Origin {
        path,
        parent: Some(parent.name.clone()),
        backend,
    })
}

/// Persist the intent to create `name`, re-checking the name under the lock.
fn record_intent(name: &str, backend: &dyn WorkspaceBackend) -> Result {
    let intent = PendingCreate {
        name: name.to_string(),
        path: pending_create_path(backend.kind(), name),
        backend_kind: backend.kind(),
        pid: std::process::id(),
        identity: watch::identity(std::process::id()),
        unknown_fields: serde_json::Map::new(),
    };
    State::with_state(|state| {
        ensure_name_free(state, name)?;
        state.pending_creates.push(intent);
        Ok(())
    })
}

fn clear_intent(state: &mut State, name: &str) {
    state.pending_creates.retain(|pending| pending.name != name);
}

fn ensure_name_free(state: &State, name: &str) -> Result {
    if state.name_in_use(name) {
        return Err(ForkspaceError::WorkspaceExists(name.to_string()));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result {
    let invalid = name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.bytes().any(|byte| byte == 0 || byte == b'/');
    if invalid {
        return Err(ForkspaceError::InvalidArgument(
            "invalid workspace name; use 1-255 bytes without '/', NUL, '.' or '..'".to_string(),
        ));
    }
    Ok(())
}
