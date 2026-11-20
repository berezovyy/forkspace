//! Background manifest builder (macOS only).
//!
//! An APFS clone is near-instant, so `new` does not wait for the manifest walk. It
//! re-executes the forkspace binary as a detached worker instead: [`spawn`] passes the job
//! through `FORKSPACE_INTERNAL_MANIFEST_*` environment variables, and `main` hands control to
//! [`run_worker_from_env`] before parsing arguments.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{ForkspaceError, Result};

#[cfg(target_os = "macos")]
const WORKER_FLAG: &str = "FORKSPACE_INTERNAL_MANIFEST_WORKER";
#[cfg(target_os = "macos")]
const WORKER_NAME: &str = "FORKSPACE_INTERNAL_MANIFEST_NAME";
#[cfg(target_os = "macos")]
const WORKER_VIEW: &str = "FORKSPACE_INTERNAL_MANIFEST_VIEW";
#[cfg(target_os = "macos")]
const WORKER_BASE: &str = "FORKSPACE_INTERNAL_MANIFEST_BASE";
#[cfg(target_os = "macos")]
const WORKER_FORK_NS: &str = "FORKSPACE_INTERNAL_MANIFEST_FORK_NS";
/// The binary [`spawn`] starts as the builder, when not this one; see
/// [`set_builder_executable`].
static BUILDER_EXECUTABLE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Start background builders from `forkspace`, the forkspace binary, instead of the running
/// one. A builder re-runs its binary as forkspace, so a program that uses forkspace as a
/// library (a test harness, an embedding tool) names forkspace's binary here.
pub fn set_builder_executable(forkspace: PathBuf) {
    *BUILDER_EXECUTABLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(forkspace);
}

/// Whether this process can start a background builder: on macOS, unless it is one.
pub(crate) fn can_spawn() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os(WORKER_FLAG).is_none()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Start a detached process that builds the manifest of workspace `name` as forked at
/// `fork_time_ns` (see [`super::stage`]), and return it so its liveness can be recorded.
#[cfg(target_os = "macos")]
pub fn spawn(
    name: String,
    view: PathBuf,
    base: PathBuf,
    fork_time_ns: u128,
) -> Result<super::Builder> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use crate::util::IoContext;

    // A worker never starts another, so a binary that ignores the worker variables cannot
    // re-run itself without end.
    if std::env::var_os(WORKER_FLAG).is_some() {
        return Err(ForkspaceError::StateFailed(
            "a background manifest builder cannot start another".to_string(),
        ));
    }
    let configured = BUILDER_EXECUTABLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let executable = match configured {
        Some(executable) => executable,
        None => std::env::current_exe()
            .io_context(|| "locate forkspace executable for manifest builder".to_string())?,
    };
    let mut command = Command::new(executable);
    command
        .env(WORKER_FLAG, "1")
        .env(WORKER_NAME, name)
        .env(WORKER_VIEW, view)
        .env(WORKER_BASE, base)
        .env(WORKER_FORK_NS, fork_time_ns.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the hook runs between fork and exec and only calls `setsid`, which is
    // async-signal-safe. A new session detaches the builder from the terminal so it
    // survives the CLI exiting and does not receive its job-control signals.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .io_context(|| "start background manifest builder".to_string())?;
    let builder = super::Builder::of(child.id());
    // Reap it if it ends while this command still runs, so a crashed builder is not taken for
    // a live one (a zombie) by this process's later checks.
    let _ = std::thread::Builder::new()
        .name("manifest-builder-reaper".to_string())
        .spawn(move || child.wait());
    Ok(builder)
}

#[cfg(not(target_os = "macos"))]
pub fn spawn(
    _name: String,
    _view: PathBuf,
    _base: PathBuf,
    _fork_time_ns: u128,
) -> Result<super::Builder> {
    Err(ForkspaceError::CapabilityUnsupported {
        diagnosis: "background clone manifests are macOS-only".to_string(),
        remedy: "use the selected Linux backend".to_string(),
    })
}

/// If this process was started by [`spawn`], build the manifest and return the outcome;
/// otherwise return `None` so `main` proceeds with normal CLI parsing.
#[cfg(target_os = "macos")]
pub fn run_worker_from_env() -> Option<Result> {
    std::env::var_os(WORKER_FLAG)?;
    Some(job_from_env().map(|(name, view, base, fork_time_ns)| {
        build_in_background(&name, &view, &base, fork_time_ns)
    }))
}

#[cfg(not(target_os = "macos"))]
pub fn run_worker_from_env() -> Option<Result> {
    None
}

#[cfg(target_os = "macos")]
fn job_from_env() -> Result<(String, PathBuf, PathBuf, u128)> {
    let name = std::env::var(WORKER_NAME).map_err(|_| {
        ForkspaceError::StateFailed("manifest worker is missing its workspace name".to_string())
    })?;
    let view = std::env::var_os(WORKER_VIEW)
        .map(PathBuf::from)
        .ok_or_else(|| {
            ForkspaceError::StateFailed("manifest worker is missing its view path".to_string())
        })?;
    let base = std::env::var_os(WORKER_BASE)
        .map(PathBuf::from)
        .ok_or_else(|| {
            ForkspaceError::StateFailed("manifest worker is missing its base path".to_string())
        })?;
    let fork_time_ns = std::env::var(WORKER_FORK_NS)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            ForkspaceError::StateFailed("manifest worker has an invalid fork time".to_string())
        })?;
    Ok((name, view, base, fork_time_ns))
}

/// Build the manifest and record the outcome. Failures are recorded as `Degraded` rather
/// than reported, since nobody is waiting on this process; a command that needs the manifest
/// tries again (see [`super::settle`]). The workspace is recorded before its builder starts;
/// a builder whose workspace is gone, belongs to another fork of that name, or no longer
/// waits for a manifest has nothing to do.
#[cfg(target_os = "macos")]
fn build_in_background(
    name: &str,
    _view: &std::path::Path,
    _base: &std::path::Path,
    fork_time_ns: u128,
) {
    use crate::state::State;

    let expected = |workspace: &crate::workspace::Workspace| {
        workspace.fork_time_ns == fork_time_ns
            && workspace.manifest_status == super::ManifestStatus::Building
    };
    let Some(workspace) = State::peek().ok().and_then(|state| {
        state
            .workspaces
            .get(name)
            .filter(|ws| expected(ws))
            .cloned()
    }) else {
        return;
    };
    match super::stage(&workspace) {
        Ok(staged) => {
            let temporary = staged.temporary.clone();
            if State::with_state(|state| super::install(state, staged)).is_err() {
                let _ = std::fs::remove_file(temporary);
            }
        }
        Err(_) => {
            let _ = State::with_state(|state| {
                if let Some(workspace) = state.workspaces.get_mut(name).filter(|ws| expected(ws)) {
                    workspace.manifest_status = super::ManifestStatus::Degraded;
                    workspace.manifest_builder = None;
                }
                Ok(())
            });
        }
    }
}
