//! `forkspace new`: create a workspace from a directory or stack one on another workspace.

use std::path::{Path, PathBuf};

use crate::backend::BackendKind;
use crate::cli::NewArgs;
use crate::error::{ForkspaceError, Result};
use crate::git;
use crate::state::State;
use crate::workspace::{self, CreateOptions, Workspace};

pub fn exec(args: NewArgs) -> Result {
    let limits = super::limit::from_flags(args.memory.as_deref(), args.cpu.as_deref())?;
    let source = Source::here()?;
    // Only a project whose own folder holds `.git` gets a branch: the copy then has a
    // repository of its own. A folder inside some other repository (one Git ignores, say)
    // must never have `git switch` run in that outer repository.
    let repository = source.directory.join(".git").exists()
        && git::toplevel(&source.directory).is_some_and(|top| top == source.directory);
    let name = match args.name {
        Some(name) => {
            workspace::validate_name(&name)?;
            if repository {
                ensure_branch_free(&source.directory, &name)?;
            }
            name
        }
        None => next_free_name(&source, repository)?,
    };
    let branch = (repository && git::validate_branch(&source.directory, &name).is_ok())
        .then(|| name.clone());
    if repository && branch.is_none() {
        eprintln!("note: '{name}' is not a valid Git branch name, so the workspace stays on the project's branch");
    }
    let ws = workspace::create(
        source.from,
        &name,
        CreateOptions {
            branch,
            ..Default::default()
        },
    )?;
    if !limits.is_empty() {
        super::limit::warn_without_systemd(&limits);
        State::with_state(|state| {
            state.require_workspace_mut(&ws.name)?.limits = limits;
            Ok(())
        })?;
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

/// What `new` copies: the workspace you are in, else the Git project you are in (its top
/// folder), else the folder you are in.
struct Source {
    /// Passed to [`workspace::create`]: a workspace's name or a directory.
    from: PathBuf,
    /// The directory holding those files, for Git.
    directory: PathBuf,
    /// The start of a name made up for the new workspace.
    label: String,
}

impl Source {
    fn here() -> Result<Self> {
        let here = std::env::current_dir()
            .and_then(|directory| directory.canonicalize())
            .map_err(|error| ForkspaceError::InvalidArgument(format!("current folder: {error}")))?;
        let state = State::peek()?;
        if let Some(name) = workspace::containing(&state, &here) {
            let directory = state.require_workspace(&name)?.view_path.clone();
            return Ok(Self {
                from: PathBuf::from(&name),
                directory,
                label: name,
            });
        }
        let directory = git::toplevel(&here).unwrap_or(here);
        let label = directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_string();
        Ok(Self {
            from: directory.clone(),
            directory,
            label,
        })
    }
}

/// `<label>-1`, `<label>-2`, ...: the first name no workspace has and, in a Git project, no
/// branch has either, since the workspace gets a branch of that name.
fn next_free_name(source: &Source, repository: bool) -> Result<String> {
    let state = State::peek()?;
    (1..)
        .map(|number| format!("{}-{number}", source.label))
        .find(|name| {
            !state.name_in_use(name)
                && !(repository
                    && git::ref_exists(&source.directory, &format!("refs/heads/{name}")))
        })
        .ok_or_else(|| ForkspaceError::InvalidArgument("no free workspace name".to_string()))
}

/// A workspace of a Git project gets a branch of its name, so that name must be free.
fn ensure_branch_free(directory: &Path, name: &str) -> Result {
    if git::ref_exists(directory, &format!("refs/heads/{name}")) {
        return Err(ForkspaceError::BranchExists(format!(
            "the project already has a branch '{name}'; pick another workspace name"
        )));
    }
    Ok(())
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
        } else if matches!(
            ws.backend_kind,
            BackendKind::LinuxReflink | BackendKind::Overlay
        ) {
            eprintln!("note: {reason}");
        }
    }
}
