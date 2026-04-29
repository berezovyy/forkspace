use crate::backend::BackendKind;
use crate::cli::ForkArgs;
use crate::error::{ForkspaceError, Result};
use crate::git;
use crate::state::State;
use crate::workspace::{self, CreateOptions};

pub fn exec(args: ForkArgs) -> Result {
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
        if !git::is_repo(base) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: format!("{} is not a Git repository", base.display()),
                remedy: "omit --branch, or run `git init` first".to_string(),
            });
        }
        if git::git_dir_is_file(base) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "--branch cannot fork a Git worktree or submodule checkout".to_string(),
                remedy: "fork the main checkout instead".to_string(),
            });
        }
        git::validate_branch(base, branch)?;
        let reference = format!("refs/heads/{branch}");
        if git::ref_exists(base, &reference) {
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
    let mut ws = workspace::create(
        args.base,
        &name,
        CreateOptions {
            session: args.session,
            tmpfs: args.tmpfs,
            tmpfs_size: args.size,
            force_copy: args.copy,
        },
    )?;
    if let Some(branch) = args.branch {
        if git::run_git(&ws.view_path, &["switch", "-c", &branch]).is_err() {
            State::with_state(|state| workspace::remove(state, &ws.name))?;
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
    if let Some(reason) = &ws.probe_reason {
        if ws.degraded {
            eprintln!("warning: {reason}");
        } else if ws.backend_kind == BackendKind::LinuxReflink {
            eprintln!("note: {reason}");
        }
    }
    if std::env::var_os("FORKSPACE_BACKEND").is_some() {
        eprintln!(
            "note: FORKSPACE_BACKEND override selected '{}'",
            ws.backend_kind.as_str()
        );
    }
    println!(
        "created workspace '{}' from {}",
        ws.name,
        ws.view_path.display()
    );
    Ok(())
}
