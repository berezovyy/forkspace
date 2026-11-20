//! `forkspace enter`: open a shell (or run a command) inside a workspace, optionally creating it
//! first and merging, dropping, or discarding it on exit.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Read as _};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use crate::backend::{select_backend, WorkspaceBackend};
use crate::cli::EnterArgs;
use crate::error::{ForkspaceError, Result};
use crate::process::sandbox;
use crate::state::State;
use crate::sync::{merge, plan};
use crate::util::{now_unix_ns, IoContext};
use crate::workspace::{self, guard, CreateOptions};

pub fn exec(args: EnterArgs) -> Result {
    let name = if args.ephemeral || args.create {
        create_for_entry(&args)?
    } else {
        workspace::pick(args.name.clone())?
    };

    let state = State::load()?;
    let workspace = state.require_workspace(&name)?;
    guard::require_live_view(&state, workspace)?;
    guard::ensure_idle(&name)?;
    let backend = select_backend(Some(workspace.backend_kind))?;

    if args.sandbox_info {
        print_sandbox(workspace, args.no_sandbox);
        return Ok(());
    }
    let mut command = shell_command(&args, &state, workspace, backend.as_ref())?;
    // Held until the command exits: builds in every workspace share its job slots.
    let jobserver = crate::process::jobserver::join();
    if let Some(holder) = &jobserver {
        command.env("CARGO_MAKEFLAGS", holder.makeflags());
    }
    // A command (rather than an interactive shell) runs at background priority on macOS.
    let background = !args.command.is_empty();
    let mut command = crate::process::systemd::scoped(command, workspace, background);
    if !args.no_sandbox {
        command = sandbox::apply(command, workspace);
    }
    let status = command
        .status()
        .io_context(|| "failed to spawn command".to_string())?;
    drop(jobserver);
    report_exit(&name, status);
    // The command's own failure is forkspace's exit status, so `forkspace enter ws -- make
    // test` fails when the tests do; cleanup below still happens first.
    let failed = (!status.success()).then(|| exit_code(status));

    let mut unmerged = false;
    if args.merge_on_exit {
        match resolve_changes_on_exit(&name, backend.as_ref()) {
            Ok(Outcome::Removed) => {
                return failed.map_or(Ok(()), |code| Err(ForkspaceError::CommandExited(code)));
            }
            Ok(Outcome::Kept) => unmerged = true,
            Ok(Outcome::Clean) => {}
            Err(error) => {
                // The merge did not complete: an ephemeral workspace still holds the work.
                if args.ephemeral {
                    eprintln!("kept ephemeral workspace '{name}': its changes were not merged");
                }
                return Err(error);
            }
        }
    }
    if args.ephemeral && unmerged {
        // Asked to merge on exit, and the changes were not merged: dropping them would lose
        // the session's work (MERGE-29).
        eprintln!(
            "kept ephemeral workspace '{name}' with its unmerged changes; merge it with `forkspace merge {name}` or remove it with `forkspace drop {name}`"
        );
    } else if args.ephemeral {
        drop_workspace(&name)?;
        println!("ephemeral workspace removed");
    }
    failed.map_or(Ok(()), |code| Err(ForkspaceError::CommandExited(code)))
}

/// A shell's exit code for `status`: the program's own, or 128 plus the signal that ended it.
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// `--sandbox-info`: where commands in `workspace` may write.
fn print_sandbox(workspace: &workspace::Workspace, no_sandbox: bool) {
    if no_sandbox {
        println!("the write sandbox is off: commands may write anywhere you can");
        return;
    }
    println!("commands in '{}' may write beneath:", workspace.name);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut hidden = 0;
    for path in sandbox::writable(workspace) {
        let in_home = home
            .as_ref()
            .is_some_and(|home| path.parent() == Some(home.as_path()));
        if in_home {
            hidden += 1;
        } else {
            println!("  {}", path.display());
        }
    }
    if hidden > 0 {
        println!("  ~/.* ({hidden} hidden entries in your home folder, except FORKSPACE_HOME)");
    }
    println!(
        "writes anywhere else, including {}, are refused",
        workspace.base.display()
    );
}

/// Create the workspace for `--new` / `--ephemeral` and return its name.
fn create_for_entry(args: &EnterArgs) -> Result<String> {
    let name = match (&args.name, args.ephemeral) {
        (Some(name), _) => name.clone(),
        (None, true) => ephemeral_name(),
        (None, false) => {
            return Err(ForkspaceError::InvalidArgument(
                "workspace name required when using --new".to_string(),
            ))
        }
    };
    let base = PathBuf::from(args.from.as_deref().unwrap_or("."));

    let created = workspace::create(base, &name, CreateOptions::default())?;
    if args.ephemeral {
        State::with_state(|state| {
            if let Some(stored) = state.workspaces.get_mut(&name) {
                stored.ephemeral = true;
            }
            Ok(())
        })?;
    }
    println!(
        "created workspace '{}' at {}",
        created.name,
        created.view_path.display()
    );
    super::new::warn_inherited_venvs(&created);
    Ok(name)
}

/// A name unlikely to collide: `ephemeral-` plus 8 hex digits hashed from time and PID.
fn ephemeral_name() -> String {
    let mut hasher = DefaultHasher::new();
    now_unix_ns().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("ephemeral-{:08x}", hasher.finish() as u32)
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
    if let Some(session) = args.session.as_ref().or(workspace.session.as_ref()) {
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

#[derive(PartialEq, Eq)]
enum Outcome {
    /// Nothing to merge.
    Clean,
    /// Changes left in the workspace, unmerged.
    Kept,
    Removed,
}

/// `--merge-on-exit`: show the pending changes and ask whether to merge, drop, or keep.
/// Without a terminal on stdin the workspace is kept.
fn resolve_changes_on_exit(name: &str, backend: &dyn WorkspaceBackend) -> Result<Outcome> {
    let state = State::load()?;
    let workspace = state.require_workspace(name)?;
    if backend.changes(workspace, false, false)?.is_empty() {
        println!("clean — no changes to merge");
        return Ok(Outcome::Clean);
    }

    println!("{}", plan::render(&plan::build(workspace, true)?));
    match prompt_choice() {
        Some('m') => {
            merge::merge_workspace(workspace)?;
            println!("merged changes into base");
            drop_workspace(name)?;
            println!("removed '{}' after merge", name);
            Ok(Outcome::Removed)
        }
        Some('d') => {
            drop_workspace(name)?;
            println!("removed");
            Ok(Outcome::Removed)
        }
        _ => {
            println!("keeping workspace");
            Ok(Outcome::Kept)
        }
    }
}

/// Ask `[m]erge  [d]rop  [k]eep` and read one byte, or `None` if stdin is not a terminal.
fn prompt_choice() -> Option<char> {
    if !io::stdin().is_terminal() {
        return None;
    }
    eprint!("[m]erge  [d]rop  [k]eep workspace? ");
    let choice = read_single_char();
    eprintln!();
    choice
}

fn read_single_char() -> Option<char> {
    let mut buf = [0u8; 1];
    io::stdin().read_exact(&mut buf).ok()?;
    Some(char::from(buf[0]))
}

fn drop_workspace(name: &str) -> Result {
    // An entered workspace is disposable: what the session left running goes with it.
    State::with_state(|state| {
        workspace::remove::remove_with(state, name, workspace::remove::Processes::Kill)
    })
}
