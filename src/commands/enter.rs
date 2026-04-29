use std::io::{self, Read as _};
use std::path::PathBuf;
use std::process::Command;

use crate::backend::select_backend;
use crate::commands::{drop as drop_cmd, fork};
use crate::error::{ForkspaceError, Result};
use crate::merge;
use crate::state::State;
use crate::util::IoContext;

fn generate_ephemeral_name() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;

    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let hash = hasher.finish();
    format!("ephemeral-{:08x}", hash as u32)
}

fn read_single_char() -> Option<char> {
    let mut buf = [0u8; 1];
    match io::stdin().read_exact(&mut buf) {
        Ok(()) => Some(buf[0] as char),
        Err(_) => None,
    }
}

fn drop_workspace_by_name(name: &str) -> Result {
    State::with_state(|state| {
        drop_cmd::remove_workspace(state, name)?;
        Ok(())
    })
}

pub fn exec(args: crate::cli::EnterArgs) -> Result {
    let ws_name = if args.ephemeral || args.create {
        let name = if let Some(n) = args.name.clone() {
            n
        } else if args.ephemeral {
            generate_ephemeral_name()
        } else {
            return Err(ForkspaceError::InvalidArgument(
                "workspace name required when using --new".to_string(),
            ));
        };

        let base = args
            .from
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let ws =
            fork::create_workspace_with_copy(base, &name, None, None, args.tmpfs, None, args.copy)?;

        if args.ephemeral {
            State::with_state(|state| {
                if let Some(stored) = state.workspaces.get_mut(&name) {
                    stored.ephemeral = true;
                }
                Ok(())
            })?;
        }

        println!(
            "created workspace '{}' from {}",
            ws.name,
            ws.view_path.display()
        );

        name
    } else {
        args.name.clone().ok_or_else(|| {
            ForkspaceError::InvalidArgument(
                "workspace name required — use --new to create one".to_string(),
            )
        })?
    };

    let state = State::load()?;
    let workspace = state.require_workspace(&ws_name)?;
    crate::reconcile::require_view(workspace)?;
    crate::reconcile::guard_merge(&ws_name)?;
    crate::pull::guard(&ws_name)?;
    let backend = select_backend(Some(workspace.backend_kind))?;

    let mut cmd = if !args.command.is_empty() {
        let mut c = Command::new(&args.command[0]);
        c.args(&args.command[1..]);
        c
    } else {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Command::new(shell)
    };

    cmd.current_dir(backend.view_path(workspace))
        .env("FORKSPACE_WORKSPACE", &workspace.name)
        .env("FORKSPACE_BASE", &workspace.base);
    for (key, value) in backend.shell_env(workspace) {
        cmd.env(key, value);
    }

    if let Some(ref sess) = args.session.or_else(|| workspace.session.clone()) {
        cmd.env("FORKSPACE_SESSION", sess);
    }

    let status = cmd
        .status()
        .io_context(|| "failed to spawn command".to_string())?;

    if status.success() {
        println!("left '{}'", ws_name);
    } else {
        let code = status.code().unwrap_or(-1);
        eprintln!("left '{}' (exit code {})", ws_name, code);
    }

    if args.merge_on_exit {
        let state = State::load()?;
        let workspace = state.require_workspace(&ws_name)?;
        let changes = backend.changes(workspace, false, false)?;

        if changes.is_empty() {
            println!("clean — no changes to merge");
        } else {
            let preview = crate::merge_plan::build(workspace, true)?;
            println!("{}", crate::merge_plan::render(&preview));
            let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
            let choice = if interactive {
                eprint!("[m]erge  [d]rop  [k]eep workspace? ");
                let choice = read_single_char().unwrap_or('k');
                eprintln!();
                choice
            } else {
                'k'
            };

            match choice {
                'm' => {
                    merge::merge_workspace(workspace)?;
                    println!("merged changes into base");
                    drop_workspace_by_name(&ws_name)?;
                    println!("removed '{}' after merge", ws_name);
                    return Ok(());
                }
                'd' => {
                    drop_workspace_by_name(&ws_name)?;
                    println!("removed");
                    return Ok(());
                }
                _ => {
                    println!("keeping workspace");
                }
            }
        }
    }

    if args.ephemeral {
        drop_workspace_by_name(&ws_name)?;
        println!("ephemeral workspace removed");
    }

    Ok(())
}
