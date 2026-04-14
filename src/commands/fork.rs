use std::path::PathBuf;

use crate::backend::{select_backend, CreateReq};
use crate::error::{ForkspaceError, Result};
use crate::state::State;
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
    let view = backend.create(&CreateReq {
        name: name.to_string(),
        base: resolved_base.clone(),
        tmpfs,
        tmpfs_size: size,
    })?;
    let ws = Workspace::from_view(
        name.to_string(),
        resolved_base,
        resolved_parent,
        session,
        backend.kind(),
        view,
    );

    State::with_state(|state| {
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
