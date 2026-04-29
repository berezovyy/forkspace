//! Creating a workspace: resolve the origin, pick a backend, and materialize the view.
//!
//! Creation is recorded as a pending intent in state before any files are written, so a
//! crash mid-materialization leaves a record that recovery can clean up.

use std::path::PathBuf;

use crate::backend::{pending_create_path, select_backend, select_backend_for_create, CreateReq};
use crate::error::{ForkspaceError, Result};
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

/// Fork `base` into a new workspace called `name`.
///
/// If `base` is the name of an existing workspace, the new workspace is stacked on top of it
/// and inherits its backend (unless `force_copy` is set).
pub fn create(base: PathBuf, name: &str, options: CreateOptions) -> Result<Workspace> {
    validate_name(name)?;
    let (origin, parent, backend) = {
        let state = State::load()?;
        if state.workspaces.contains_key(name) || state.has_pending_create(name) {
            return Err(ForkspaceError::WorkspaceExists(name.to_string()));
        }
        match state.workspaces.get(base.to_str().unwrap_or("")) {
            Some(parent) => {
                guard::ensure_idle(&parent.name)?;
                if state.depth_of(&parent.name) + 1 >= MAX_FORK_DEPTH {
                    return Err(ForkspaceError::InvalidArgument(format!(
                        "workspace stack depth is limited to {MAX_FORK_DEPTH}; merge or fork from an earlier ancestor"
                    )));
                }
                let origin = select_backend(Some(parent.backend_kind))?.view_path(parent);
                let backend = if options.force_copy {
                    select_backend_for_create(&origin, true)?
                } else {
                    select_backend(Some(parent.backend_kind))?
                };
                (origin, Some(parent.name.clone()), backend)
            }
            None => {
                let backend = select_backend_for_create(&base, options.force_copy)?;
                (base, None, backend)
            }
        }
    };

    backend.preflight(&origin)?;
    let origin = origin
        .canonicalize()
        .io_context(|| format!("failed to canonicalize base path: {}", origin.display()))?;

    let intent = PendingCreate {
        name: name.to_string(),
        path: pending_create_path(backend.kind(), name),
        backend_kind: backend.kind(),
        pid: std::process::id(),
        identity: crate::process::watch::identity(std::process::id()),
        unknown_fields: serde_json::Map::new(),
    };
    State::with_state(|state| {
        if state.workspaces.contains_key(name) || state.has_pending_create(name) {
            return Err(ForkspaceError::WorkspaceExists(name.to_string()));
        }
        state.pending_creates.push(intent);
        Ok(())
    })?;

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
                state.pending_creates.retain(|pending| pending.name != name);
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
        state.pending_creates.retain(|pending| pending.name != name);
        state.add_workspace(workspace.clone())
    })?;
    Ok(workspace)
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
