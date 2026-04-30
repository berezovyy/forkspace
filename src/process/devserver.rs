//! Dev servers started by `forkspace run`: port choice, log files, the detached process group,
//! and routing the proxy to the active one.

use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;
use std::process::Command;

use serde_json::Map;

use super::spawn::{create_logs, spawn_detached};
use super::watch::{self, IDENTITY_ENV};
use super::{port_is_free, proxy};
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
) -> Result<Started> {
    let workspace = state.require_workspace(name)?;
    guard::require_view(workspace)?;
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

    let process = spawn(name, command, &view, &env, port)?;
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
pub(crate) fn switch_to(state: &mut State, name: &str) -> Result<u16> {
    let port = state
        .require_workspace(name)?
        .process
        .as_ref()
        .filter(|process| process.is_alive())
        .map(|process| process.port)
        .ok_or_else(|| ForkspaceError::NoProcessRunning(name.to_string()))?;
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
    name: &str,
    command: &str,
    view: &Path,
    env: &[(String, String)],
    port: u16,
) -> Result<RunningProcess> {
    let log_dir = paths::log_dir(name);
    fs::create_dir_all(&log_dir)
        .map_err(|error| ForkspaceError::ProcessFailed(format!("create log directory: {error}")))?;
    let (stdout, stderr) = create_logs(&log_dir, "stdout.log", "stderr.log")
        .map_err(|e| ForkspaceError::ProcessFailed(format!("log file: {}", e)))?;

    let identity = watch::new_identity();
    let child = spawn_detached(
        Command::new("sh")
            .args(["-c", command])
            .current_dir(view)
            .envs(env.iter().map(|(key, value)| (key, value)))
            .env("PORT", port.to_string())
            .env(IDENTITY_ENV, &identity)
            .stdout(stdout)
            .stderr(stderr),
    )
    .map_err(|e| ForkspaceError::ProcessFailed(format!("spawn: {}", e)))?;

    Ok(RunningProcess {
        pid: child.id(),
        identity: Some(identity),
        command: command.to_string(),
        port,
        unknown_fields: Map::new(),
    })
}
