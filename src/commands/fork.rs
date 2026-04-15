use std::path::PathBuf;

use crate::backend::{pending_create_path, select_backend, CreateReq};
use crate::error::{ForkspaceError, Result};
use crate::state::{PendingCreate, State};
use crate::util::IoContext;
use crate::workspace::Workspace;

pub fn create_workspace(
    base: PathBuf,
    name: &str,
    parent: Option<String>,
    session: Option<String>,
    tmpfs: bool,
    size: Option<String>,
) -> Result<Workspace> {
    validate_workspace_name(name)?;
    let (resolved_base, resolved_parent, backend) = {
        let state = State::load()?;
        if state.workspaces.contains_key(name) {
            return Err(ForkspaceError::WorkspaceExists(name.to_string()));
        }
        if state
            .pending_creates
            .iter()
            .any(|pending| pending.name == name)
        {
            return Err(ForkspaceError::WorkspaceExists(name.to_string()));
        }
        if let Some(ws) = state.workspaces.get(base.to_str().unwrap_or("")) {
            let backend = select_backend(Some(ws.backend_kind))?;
            (backend.view_path(ws), Some(ws.name.clone()), backend)
        } else {
            (base.clone(), parent, select_backend(None)?)
        }
    };

    backend.preflight(&resolved_base)?;

    let resolved_base = resolved_base.canonicalize().io_context(|| {
        format!(
            "failed to canonicalize base path: {}",
            resolved_base.display()
        )
    })?;
    let request = CreateReq {
        name: name.to_string(),
        base: resolved_base.clone(),
        tmpfs,
        tmpfs_size: size,
    };
    let intent = PendingCreate {
        name: name.to_string(),
        path: pending_create_path(backend.kind(), name),
        backend_kind: backend.kind(),
        pid: std::process::id(),
        unknown_fields: serde_json::Map::new(),
    };
    State::with_state(|state| {
        if state.workspaces.contains_key(name)
            || state
                .pending_creates
                .iter()
                .any(|pending| pending.name == name)
        {
            return Err(ForkspaceError::WorkspaceExists(name.to_string()));
        }
        state.pending_creates.push(intent);
        Ok(())
    })?;
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
    let ws = Workspace::from_view(
        name.to_string(),
        resolved_base,
        resolved_parent,
        session,
        backend.kind(),
        view,
    );

    State::with_state(|state| {
        state.pending_creates.retain(|pending| pending.name != name);
        state.add_workspace(ws.clone())?;
        Ok(())
    })?;

    Ok(ws)
}

fn validate_workspace_name(name: &str) -> Result {
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

pub fn exec(args: crate::cli::ForkArgs) -> Result {
    let name = match args.name {
        Some(n) => n,
        None => args
            .base
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                ForkspaceError::InvalidArgument(
                    "cannot derive workspace name from path; use --name".to_string(),
                )
            })?,
    };
    let ws = create_workspace(args.base, &name, None, args.session, args.tmpfs, args.size)?;
    println!(
        "created workspace '{}' from {}",
        ws.name,
        ws.view_path.display()
    );
    Ok(())
}
