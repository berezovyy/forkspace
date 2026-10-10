//! Dev servers started by `forkspace run`: port choice, log files, the detached process group,
//! and routing the proxy to the active one.

use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Map;

use super::spawn::{create_logs, spawn_detached};
use super::watch::{self, IDENTITY_ENV};
use super::{port_is_free, ports, proxy};
use crate::backend::select_backend;
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::state::State;
use crate::workspace::{guard, RunningProcess};

/// Ports tried in order when the requested one is taken (see [`ForkspaceError::PortRangeExhausted`]).
const FALLBACK_PORTS: RangeInclusive<u16> = 5501..=5600;

/// A dev server that was started and is now the proxy's target.
pub(crate) struct Started {
    pub process: RunningProcess,
    /// Port the proxy listens on.
    pub proxy_port: u16,
}

/// Start `command` in workspace `name`'s view and route the proxy to it.
///
/// The app receives `requested_port` in `$PORT` unless another workspace has recorded that
/// port or something is already listening on it; then the first free fallback port is used.
/// If the proxy cannot be started, the new dev server is stopped again.
pub(crate) fn start(
    state: &mut State,
    name: &str,
    command: &str,
    requested_port: u16,
    sandbox: bool,
) -> Result<Started> {
    let workspace = state.require_workspace(name)?;
    guard::require_live_view(state, workspace)?;
    if workspace
        .process
        .as_ref()
        .is_some_and(RunningProcess::is_alive)
    {
        return Err(ForkspaceError::ProcessAlreadyRunning(name.to_string()));
    }
    let port = choose_port(state, requested_port)?;
    let view = select_backend(Some(workspace.backend_kind))?.view_path(workspace);
    let env = crate::workspace::process_env(state, workspace)?;

    let process = spawn(workspace, command, &view, &env, port, sandbox)?;
    state.require_workspace_mut(name)?.process = Some(process.clone());

    let proxy = match proxy::ensure_running(state) {
        Ok(proxy) => proxy,
        Err(error) => {
            process.terminate();
            state.require_workspace_mut(name)?.process = None;
            return Err(error);
        }
    };
    proxy.route_to(name, port);
    Ok(Started {
        proxy_port: proxy.listen_port,
        process,
    })
}

/// What [`discover_port`] saw while waiting for the dev server to listen.
pub(crate) enum PortDiscovery {
    /// The server listens on the port it was given in `$PORT`.
    Assigned(u16),
    /// The server ignored `$PORT`; its record and the proxy now point at `actual`.
    Moved { assigned: u16, actual: u16 },
    /// Nothing listened within the wait; the proxy keeps forwarding to `$PORT`.
    NotListening(u16),
    /// The server exited before it listened; its output is in `log_dir`.
    Exited { log_dir: PathBuf },
}

/// Wait up to `wait` for workspace `name`'s dev server to listen, and route to the port it
/// actually bound. Runs without holding the state lock while it waits.
pub(crate) fn discover_port(name: &str, wait: Duration) -> Result<PortDiscovery> {
    let process = State::load()?
        .require_workspace(name)?
        .process
        .clone()
        .ok_or_else(|| ForkspaceError::NoProcessRunning(name.to_string()))?;
    let deadline = Instant::now() + wait;
    let actual = loop {
        if !process.is_alive() {
            return Ok(PortDiscovery::Exited {
                log_dir: paths::log_dir(name),
            });
        }
        if let Some(port) = ports::pick_port(&ports::listening_ports(process.pid), process.port) {
            break port;
        }
        if Instant::now() >= deadline {
            return Ok(PortDiscovery::NotListening(process.port));
        }
        thread::sleep(Duration::from_millis(100));
    };
    if actual == process.port {
        return Ok(PortDiscovery::Assigned(actual));
    }
    State::with_state(|state| {
        reroute(state, name, process.pid, actual);
        Ok(())
    })?;
    Ok(PortDiscovery::Moved {
        assigned: process.port,
        actual,
    })
}

/// Record that `name`'s server (still `pid`) listens on `port`, and follow it with the proxy
/// if the proxy points at this workspace.
fn reroute(state: &mut State, name: &str, pid: u32, port: u16) {
    let Some(process) = state
        .workspaces
        .get_mut(name)
        .and_then(|workspace| workspace.process.as_mut())
        .filter(|process| process.pid == pid)
    else {
        return;
    };
    process.port = port;
    if let Some(proxy) = state
        .proxy
        .as_mut()
        .filter(|proxy| proxy.active_workspace.as_deref() == Some(name))
    {
        proxy.route_to(name, port);
    }
}

/// Stop workspace `name`'s dev server and forget it.
pub(crate) fn stop(state: &mut State, name: &str) -> Result<RunningProcess> {
    let process = state
        .require_workspace_mut(name)?
        .process
        .take()
        .ok_or_else(|| ForkspaceError::NoProcessRunning(name.to_string()))?;
    process.terminate();
    Ok(process)
}

/// Route the proxy to workspace `name`'s running dev server and return that server's port.
/// The recorded port is re-checked against what the server listens on now, so a server that
/// was slow to start (or ignored `$PORT`) is still reached.
pub(crate) fn switch_to(state: &mut State, name: &str) -> Result<u16> {
    let process = state
        .require_workspace(name)?
        .process
        .clone()
        .filter(RunningProcess::is_alive)
        .ok_or_else(|| ForkspaceError::NoProcessRunning(name.to_string()))?;
    let port = ports::pick_port(&ports::listening_ports(process.pid), process.port)
        .unwrap_or(process.port);
    if port != process.port {
        reroute(state, name, process.pid, port);
    }
    let proxy = state.proxy.as_mut().ok_or_else(|| {
        ForkspaceError::ProxyFailed(
            "no proxy running — start a process first with `forkspace run`".to_string(),
        )
    })?;
    proxy.route_to(name, port);
    Ok(port)
}

/// `requested` if usable, else the first usable fallback port. A port is usable if no
/// workspace has it recorded (live or not) and nothing is listening on it.
fn choose_port(state: &State, requested: u16) -> Result<u16> {
    let recorded: Vec<u16> = state
        .workspaces
        .values()
        .filter_map(|workspace| workspace.process.as_ref())
        .map(|process| process.port)
        .collect();
    std::iter::once(requested)
        .chain(FALLBACK_PORTS)
        .find(|port| !recorded.contains(port) && port_is_free(*port))
        .ok_or(ForkspaceError::PortRangeExhausted)
}

/// Launch `sh -c command` in `view` as a detached process group logging to the workspace's
/// log directory.
fn spawn(
    workspace: &crate::workspace::Workspace,
    command: &str,
    view: &Path,
    env: &[(String, String)],
    port: u16,
    sandbox: bool,
) -> Result<RunningProcess> {
    let log_dir = paths::log_dir(&workspace.name);
    fs::create_dir_all(&log_dir)
        .map_err(|error| ForkspaceError::ProcessFailed(format!("create log directory: {error}")))?;
    let (stdout, stderr) = create_logs(&log_dir, "stdout.log", "stderr.log")
        .map_err(|e| ForkspaceError::ProcessFailed(format!("log file: {}", e)))?;

    let identity = watch::new_identity();
    let mut shell = Command::new("sh");
    shell
        .args(["-c", command])
        .current_dir(view)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .env("PORT", port.to_string())
        .env(IDENTITY_ENV, &identity);
    // `systemd-run --scope` execs the command, so the recorded pid stays the dev server's.
    let mut launcher = super::systemd::scoped(shell, workspace, true);
    if sandbox && !super::sandbox::disabled_by_default() {
        launcher = super::sandbox::apply(launcher, workspace);
    }
    let child = spawn_detached(launcher.stdout(stdout).stderr(stderr))
        .map_err(|e| ForkspaceError::ProcessFailed(format!("spawn: {}", e)))?;

    Ok(RunningProcess {
        pid: child.id(),
        identity: Some(identity),
        command: command.to_string(),
        port,
        unknown_fields: Map::new(),
    })
}
