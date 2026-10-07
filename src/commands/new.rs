//! `forkspace new`: create a workspace from a directory or stack one on another workspace.

use std::path::{Path, PathBuf};

use crate::backend::BackendKind;
use crate::cli::NewArgs;
use crate::error::{ForkspaceError, Result};
use crate::git;
use crate::state::State;
use crate::workspace::{self, CreateOptions, Workspace};

pub fn exec(args: NewArgs) -> Result {
    if let Some(branch) = args.branch.as_deref() {
        ensure_branch_creatable(&branch_base(&args.from)?, branch)?;
    }
    let name = match args.name {
        Some(name) => name,
        None => default_name(&args.from)?,
    };
    let mut ws = workspace::create(
        args.from,
        &name,
        CreateOptions {
            session: args.session,
            tmpfs: args.tmpfs,
            force_copy: args.copy,
        },
    )?;
    if let Some(branch) = args.branch {
        create_branch(&mut ws, branch)?;
    }
    print_backend_notes(&ws);
    warn_inherited_venvs(&ws);
    println!(
        "created workspace '{}' at {}",
        ws.name,
        ws.view_path.display()
    );
    Ok(())
}

/// The repository a `--branch` workspace branches from: the parent's view when stacking.
fn branch_base(base: &Path) -> Result<PathBuf> {
    let state = State::load()?;
    Ok(state
        .workspaces
        .get(base.to_str().unwrap_or(""))
        .map_or_else(|| base.to_path_buf(), |parent| parent.view_path.clone()))
}

/// Check `--branch` before creating anything, so a bad branch never leaves a workspace.
fn ensure_branch_creatable(base: &Path, branch: &str) -> Result {
    if !git::is_repo(base) {
        return Err(ForkspaceError::CapabilityUnsupported {
            diagnosis: format!("{} is not a Git repository", base.display()),
            remedy: "omit --branch, or run `git init` first".to_string(),
        });
    }
    if git::git_dir_is_file(base) {
        return Err(ForkspaceError::CapabilityUnsupported {
            diagnosis: "--branch cannot start from a Git worktree or submodule checkout"
                .to_string(),
            remedy: "use the main checkout as --from instead".to_string(),
        });
    }
    git::validate_branch(base, branch)?;
    if git::ref_exists(base, &format!("refs/heads/{branch}")) {
        return Err(ForkspaceError::BranchExists(format!(
            "'{branch}' already exists; choose another branch name"
        )));
    }
    Ok(())
}

fn default_name(base: &Path) -> Result<String> {
    base.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            ForkspaceError::InvalidArgument(
                "cannot derive a workspace name from the source path; pass a name".to_string(),
            )
        })
}

/// Switch the new workspace to a fresh `branch` and record it. On failure the workspace is
/// removed again, so `--branch` is all-or-nothing.
fn create_branch(ws: &mut Workspace, branch: String) -> Result {
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
    })
}

/// Warn about Python virtual environments the fork copied whose scripts still point at the
/// base, since installing through them writes into the base.
pub(crate) fn warn_inherited_venvs(ws: &Workspace) {
    for venv in workspace::inherited_venvs(ws) {
        let path = venv.path.display();
        eprintln!(
            "warning: {path} is a Python virtual environment for {}; its scripts still run \
             from there, so installs from this workspace change it. Recreate it here with \
             `python3 -m venv --clear {path}` (or `uv venv {path}`) and reinstall.",
            venv.points_to.display()
        );
    }
}

/// Tell the user how the backend materialized the view when it is not the obvious fast path.
fn print_backend_notes(ws: &Workspace) {
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
}
