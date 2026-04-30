//! Background manifest builder (macOS only).
//!
//! An APFS clone is near-instant, so `fork` does not wait for the manifest walk. It
//! re-executes the forkspace binary as a detached worker instead: [`spawn`] passes the job
//! through `FORKSPACE_INTERNAL_MANIFEST_*` environment variables, and `main` hands control to
//! [`run_worker_from_env`] before parsing arguments.

#[cfg(target_os = "macos")]
use std::path::PathBuf;

use crate::error::{ForkspaceError, Result};

#[cfg(target_os = "macos")]
const WORKER_FLAG: &str = "FORKSPACE_INTERNAL_MANIFEST_WORKER";
#[cfg(target_os = "macos")]
const WORKER_NAME: &str = "FORKSPACE_INTERNAL_MANIFEST_NAME";
#[cfg(target_os = "macos")]
const WORKER_VIEW: &str = "FORKSPACE_INTERNAL_MANIFEST_VIEW";
#[cfg(target_os = "macos")]
const WORKER_FORK_NS: &str = "FORKSPACE_INTERNAL_MANIFEST_FORK_NS";

/// Start a detached process that builds workspace `name`'s manifest from `view`.
#[cfg(target_os = "macos")]
pub fn spawn(name: String, view: PathBuf, fork_time_ns: u128) -> Result {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use crate::util::IoContext;

    let executable = std::env::current_exe()
        .io_context(|| "locate forkspace executable for manifest builder".to_string())?;
    let mut command = Command::new(executable);
    command
        .env(WORKER_FLAG, "1")
        .env(WORKER_NAME, name)
        .env(WORKER_VIEW, view)
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
    command
        .spawn()
        .io_context(|| "start background manifest builder".to_string())?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn spawn(_name: String, _view: std::path::PathBuf, _fork_time_ns: u128) -> Result {
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
    Some(
        job_from_env()
            .map(|(name, view, fork_time_ns)| build_in_background(&name, &view, fork_time_ns)),
    )
}

#[cfg(not(target_os = "macos"))]
pub fn run_worker_from_env() -> Option<Result> {
    None
}

#[cfg(target_os = "macos")]
fn job_from_env() -> Result<(String, PathBuf, u128)> {
    let name = std::env::var(WORKER_NAME).map_err(|_| {
        ForkspaceError::StateFailed("manifest worker is missing its workspace name".to_string())
    })?;
    let view = std::env::var_os(WORKER_VIEW)
        .map(PathBuf::from)
        .ok_or_else(|| {
            ForkspaceError::StateFailed("manifest worker is missing its view path".to_string())
        })?;
    let fork_time_ns = std::env::var(WORKER_FORK_NS)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            ForkspaceError::StateFailed("manifest worker has an invalid fork time".to_string())
        })?;
    Ok((name, view, fork_time_ns))
}

/// Build the manifest and record the outcome. Failures are recorded as `Degraded` rather
/// than reported, since nobody is waiting on this process.
#[cfg(target_os = "macos")]
fn build_in_background(name: &str, view: &std::path::Path, fork_time_ns: u128) {
    if !wait_for_workspace(name) {
        return;
    }
    let status = if super::build(name, view, fork_time_ns).is_ok() {
        super::ManifestStatus::Ready
    } else {
        super::ManifestStatus::Degraded
    };
    // The workspace was dropped while we were building: discard the orphaned manifest.
    if !super::mark(name, status).unwrap_or(false) {
        let _ = super::remove(name);
    }
}

/// The forking process commits the workspace record only after spawning us, so poll for it
/// (up to about five seconds) before building anything.
#[cfg(target_os = "macos")]
fn wait_for_workspace(name: &str) -> bool {
    use crate::state::State;

    for _ in 0..500 {
        match State::load() {
            Ok(state) if state.workspaces.contains_key(name) => break,
            Ok(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => return false,
        }
    }
    State::load().is_ok_and(|state| state.workspaces.contains_key(name))
}
