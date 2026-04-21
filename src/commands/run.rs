use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::backend::select_backend;
use crate::cli::RunArgs;
use crate::error::{ForkspaceError, Result};
use crate::state::{ProxyConfig, State};
use crate::util::{forkspace_home, kill_process};
use crate::workspace::RunningProcess;

const PORT_RANGE_START: u16 = 5501;
const PORT_RANGE_END: u16 = 5600;
const PROXY_LISTEN_PORT: u16 = 4000;

fn spawn_detached(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    };
    cmd.spawn()
}

fn open_log_pair(
    dir: &Path,
    out_name: &str,
    err_name: &str,
    wrap_err: impl Fn(String) -> ForkspaceError,
) -> Result<(std::fs::File, std::fs::File)> {
    let stdout = std::fs::File::create(dir.join(out_name))
        .map_err(|e| wrap_err(format!("log file: {}", e)))?;
    let stderr = std::fs::File::create(dir.join(err_name))
        .map_err(|e| wrap_err(format!("log file: {}", e)))?;
    Ok((stdout, stderr))
}

fn is_port_available(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn allocate_port(state: &State) -> Result<u16> {
    let used_ports: Vec<u16> = state
        .workspaces
        .values()
        .filter_map(|ws| ws.process.as_ref())
        .map(|p| p.port)
        .collect();

    for port in PORT_RANGE_START..=PORT_RANGE_END {
        if !used_ports.contains(&port) && is_port_available(port) {
            return Ok(port);
        }
    }
    Err(ForkspaceError::PortRangeExhausted)
}

fn ensure_proxy(state: &mut State, listen_port: u16) -> Result {
    if let Some(ref proxy) = state.proxy {
        if let Some(pid) = proxy.proxy_pid {
            if crate::process_watch::is_alive(pid, proxy.proxy_identity.as_deref()) {
                return Ok(());
            }
        }
    }

    if !is_port_available(listen_port) {
        return Err(ForkspaceError::ProxyFailed(format!(
            "port {listen_port} is unavailable; stop the process using it and retry"
        )));
    }

    let exe = std::env::current_exe()
        .map_err(|e| ForkspaceError::ProxyFailed(format!("cannot find self: {}", e)))?;

    let log_dir = forkspace_home();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "warning: failed to create log directory {}: {e}",
            log_dir.display()
        );
    }

    let (stdout, stderr) = open_log_pair(
        &log_dir,
        "proxy.log",
        "proxy.err",
        ForkspaceError::ProxyFailed,
    )?;

    let proxy_identity = crate::process_watch::new_identity();
    let child = spawn_detached(
        Command::new(exe)
            .args(["proxy-daemon", "--port", &listen_port.to_string()])
            .env("FORKSPACE_PROCESS_IDENTITY", &proxy_identity)
            .stdout(stdout)
            .stderr(stderr),
    )
    .map_err(|e| ForkspaceError::ProxyFailed(format!("spawn proxy: {}", e)))?;

    state.proxy = Some(ProxyConfig {
        listen_port,
        active_workspace: None,
        target_port: None,
        proxy_pid: Some(child.id()),
        proxy_identity: Some(proxy_identity),
        unknown_fields: serde_json::Map::new(),
    });

    std::thread::sleep(std::time::Duration::from_millis(100));

    Ok(())
}

pub fn exec(args: RunArgs) -> Result {
    if args.stop {
        return stop(&args.name);
    }

    if args.command.is_empty() {
        return Err(ForkspaceError::InvalidArgument(
            "command required: forkspace run <workspace> -- <command>".to_string(),
        ));
    }

    State::with_state(|state| {
        let ws = state.require_workspace(&args.name)?;

        if let Some(ref proc) = ws.process {
            if crate::process_watch::is_alive(proc.pid, proc.identity.as_deref()) {
                return Err(ForkspaceError::ProcessAlreadyRunning(args.name.clone()));
            }
        }

        let requested_in_use = state
            .workspaces
            .values()
            .filter_map(|workspace| workspace.process.as_ref())
            .any(|process| process.port == args.port);
        let app_port = if !requested_in_use && is_port_available(args.port) {
            args.port
        } else {
            allocate_port(state)?
        };
        let proxy_listen_port = PROXY_LISTEN_PORT;
        let cmd = args.command.join(" ");
        let workspace = state.require_workspace(&args.name)?;
        let merged = select_backend(Some(workspace.backend_kind))?.view_path(workspace);

        let log_dir = forkspace_home().join("logs").join(&args.name);
        std::fs::create_dir_all(&log_dir).map_err(|error| {
            ForkspaceError::ProcessFailed(format!("create log directory: {error}"))
        })?;
        let (stdout, stderr) = open_log_pair(
            &log_dir,
            "stdout.log",
            "stderr.log",
            ForkspaceError::ProcessFailed,
        )?;

        let identity = crate::process_watch::new_identity();
        let child = spawn_detached(
            Command::new("sh")
                .args(["-c", &cmd])
                .current_dir(&merged)
                .env("PORT", app_port.to_string())
                .env("FORKSPACE_WORKSPACE", &args.name)
                .env("FORKSPACE_PROCESS_IDENTITY", &identity)
                .stdout(stdout)
                .stderr(stderr),
        )
        .map_err(|e| ForkspaceError::ProcessFailed(format!("spawn: {}", e)))?;

        let pid = child.id();
        let identity = Some(identity);

        let ws = state.require_workspace_mut(&args.name)?;
        ws.process = Some(RunningProcess {
            pid,
            identity: identity.clone(),
            command: cmd.clone(),
            port: app_port,
            unknown_fields: serde_json::Map::new(),
        });

        if let Err(error) = ensure_proxy(state, proxy_listen_port) {
            kill_process(pid, identity.as_deref());
            state.require_workspace_mut(&args.name)?.process = None;
            return Err(error);
        }

        if let Some(ref mut proxy) = state.proxy {
            proxy.active_workspace = Some(args.name.clone());
            proxy.target_port = Some(app_port);
        }

        let proxy_port = state
            .proxy
            .as_ref()
            .map_or(proxy_listen_port, |p| p.listen_port);
        println!("running '{}' on port {} (PID {})", cmd, app_port, pid);
        println!("proxy: http://localhost:{}", proxy_port);

        Ok(())
    })
}

fn stop(name: &str) -> Result {
    State::with_state(|state| {
        let ws = state.require_workspace_mut(name)?;

        let proc = ws
            .process
            .take()
            .ok_or_else(|| ForkspaceError::NoProcessRunning(name.to_string()))?;

        kill_process(proc.pid, proc.identity.as_deref());

        println!("stopped '{}' (PID {})", proc.command, proc.pid);
        Ok(())
    })
}
