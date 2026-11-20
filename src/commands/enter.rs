//! `forkspace enter`: open a shell (or run a command) inside a workspace, making the workspace
//! first when it does not exist yet. Commands run in the write sandbox: they can change the
//! workspace, not the project or other workspaces.

use std::process::{Command, ExitStatus};

use crate::backend::{select_backend, WorkspaceBackend};
use crate::cli::EnterArgs;
use crate::error::{ForkspaceError, Result};
use crate::process::sandbox;
use crate::state::State;
use crate::util::IoContext;
use crate::workspace::{self, guard};

pub fn exec(args: EnterArgs) -> Result {
    let name = target(args.name.clone())?;
    let state = State::load()?;
    let workspace = state.require_workspace(&name)?;
    guard::require_live_view(&state, workspace)?;
    guard::ensure_idle(&name)?;
    let backend = select_backend(Some(workspace.backend_kind))?;

    let mut command = shell_command(&args, &state, workspace, backend.as_ref())?;
    // Held until the command exits: builds in every workspace share its job slots.
    let jobserver = crate::process::jobserver::join();
    if let Some(holder) = &jobserver {
        command.env("CARGO_MAKEFLAGS", holder.makeflags());
    }
    // A command (rather than an interactive shell) runs at background priority on macOS.
    let background = !args.command.is_empty();
    let command = crate::process::systemd::scoped(command, workspace, background);
    let mut command = sandbox::apply(command, workspace);
    let status = command
        .status()
        .io_context(|| "failed to spawn command".to_string())?;
    drop(jobserver);
    report_exit(&name, status);
    // The command's own failure is forkspace's exit status, so `forkspace enter ws make test`
    // fails when the tests do.
    if status.success() {
        Ok(())
    } else {
        Err(ForkspaceError::CommandExited(exit_code(status)))
    }
}

/// The workspace to enter: the one named, made from where you are when there is none of that
/// name yet; else the one you are in or the only one; else, with none at all, a new one.
fn target(name: Option<String>) -> Result<String> {
    match name {
        Some(name) if State::peek()?.workspaces.contains_key(&name) => Ok(name),
        Some(name) => Ok(super::new::create(Some(name))?.name),
        None => match workspace::pick(None) {
            Ok(name) => Ok(name),
            Err(_) if State::peek()?.workspaces.is_empty() => Ok(super::new::create(None)?.name),
            Err(error) => Err(error),
        },
    }
}

/// A shell's exit code for `status`: the program's own, or 128 plus the signal that ended it.
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// The user's command (or `$SHELL`) set up to run in the workspace view.
fn shell_command(
    args: &EnterArgs,
    state: &State,
    workspace: &workspace::Workspace,
    backend: &dyn WorkspaceBackend,
) -> Result<Command> {
    let mut command = match args.command.split_first() {
        Some((program, rest)) => {
            let mut command = Command::new(program);
            command.args(rest);
            command
        }
        None => Command::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())),
    };
    command
        .current_dir(backend.view_path(workspace))
        .envs(workspace::process_env(state, workspace)?);
    if let Some(session) = workspace.session.as_ref() {
        command.env("FORKSPACE_SESSION", session);
    }
    Ok(command)
}

fn report_exit(name: &str, status: ExitStatus) {
    if status.success() {
        println!("left '{}'", name);
    } else {
        eprintln!("left '{}' (exit code {})", name, exit_code(status));
    }
}
