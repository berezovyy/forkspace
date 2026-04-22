use std::path::PathBuf;

use crate::backend::{pending_create_path, select_backend, CreateReq};
use crate::error::{ForkspaceError, Result};
use crate::state::{PendingCreate, State};
use crate::util::IoContext;
use crate::workspace::Workspace;

const MAX_FORK_DEPTH: usize = 32;

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
            if state.depth_of(&ws.name) + 1 >= MAX_FORK_DEPTH {
                return Err(ForkspaceError::InvalidArgument(format!(
                    "workspace stack depth is limited to {MAX_FORK_DEPTH}; merge or fork from an earlier ancestor"
                )));
            }
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
    let branch_base = if args.branch.is_some() {
        let state = State::load()?;
        Some(
            state
                .workspaces
                .get(args.base.to_str().unwrap_or(""))
                .map_or_else(
                    || args.base.clone(),
                    |workspace| workspace.view_path.clone(),
                ),
        )
    } else {
        None
    };
    if let (Some(branch), Some(base)) = (args.branch.as_deref(), branch_base.as_deref()) {
        if !crate::git::is_repo(base) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: format!("{} is not a Git repository", base.display()),
                remedy: "omit --branch, or run `git init` first".to_string(),
            });
        }
        if crate::git::git_dir_is_file(base) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "--branch cannot fork a Git worktree or submodule checkout".to_string(),
                remedy: "fork the main checkout instead".to_string(),
            });
        }
        crate::git::validate_branch(base, branch)?;
        let reference = format!("refs/heads/{branch}");
        if crate::git::ref_exists(base, &reference) {
            return Err(ForkspaceError::BranchExists(format!(
                "'{branch}' already exists; choose another branch name"
            )));
        }
    }
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
    let mut ws = create_workspace(args.base, &name, None, args.session, args.tmpfs, args.size)?;
    if let Some(branch) = args.branch {
        if crate::git::run_git(&ws.view_path, &["switch", "-c", &branch]).is_err() {
            State::with_state(|state| crate::commands::drop::remove_workspace(state, &ws.name))?;
            return Err(ForkspaceError::BranchExists(format!(
                "'{branch}' could not be created; no workspace was kept"
            )));
        }
        ws.branch = Some(branch);
        State::with_state(|state| {
            state.require_workspace_mut(&ws.name)?.branch = ws.branch.clone();
            Ok(())
        })?;
    }
    println!(
        "created workspace '{}' from {}",
        ws.name,
        ws.view_path.display()
    );
    Ok(())
}
