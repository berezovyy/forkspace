//! Refreshing the Git index of a new workspace in the background.
//!
//! A fork gives every file a new inode and ctime, so the index the workspace inherited matches
//! none of them, and the first `git status` in it reads and hashes every tracked file again:
//! 725 ms against 17 ms afterwards on a 20,000-file tree with the overlay backend, 99 ms
//! against 15 ms with a copy (2026-11-19). `new` starts `git update-index -q --refresh` in
//! the workspace at low priority as soon as it exists, so that work is done before an agent's
//! first `git status` rather than during it. The refresh only rewrites `.git/index`, never a
//! tracked file; when the agent's own `git status` holds the index lock first, the refresh
//! gives up and the status refreshes the index itself, as it would have anyway.
//!
//! The refresh is recorded in `logs/<name>/index-refresh` so that it is not counted as a
//! process using the workspace, and so that `drop` stops it before removing the view.
//! `FORKSPACE_GIT_REFRESH=0` turns it off.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::paths;
use crate::process::watch::{self, IDENTITY_ENV};
use crate::workspace::Workspace;

fn record_path(name: &str) -> PathBuf {
    paths::log_dir(name).join("index-refresh")
}

/// Whether `workspace` is an overlay whose lower layer is a frozen copy with an index already
/// refreshed against it: its view shows the same inode numbers and times, so its index is
/// fresh from the start.
fn inherits_fresh_index(workspace: &Workspace) -> bool {
    #[cfg(target_os = "linux")]
    {
        if let crate::backend::BackendState::Overlay(layers) = &workspace.backend_state {
            return layers
                .lower
                .as_deref()
                .is_some_and(crate::backend::overlay::frozen::index_refreshed);
        }
    }
    let _ = workspace;
    false
}

fn disabled() -> bool {
    std::env::var("FORKSPACE_GIT_REFRESH")
        .is_ok_and(|value| matches!(value.as_str(), "0" | "off" | "false" | "no"))
}

/// Start refreshing the index of `workspace` in the background, if its view has a Git
/// repository. Best effort: a refresh that cannot start only leaves the first `git status`
/// to do the work.
pub fn start(workspace: &Workspace) {
    if disabled() || !workspace.view_path.join(".git").exists() || inherits_fresh_index(workspace) {
        return;
    }
    let identity = watch::new_identity();
    let mut command = Command::new("git");
    command
        // No fsmonitor daemon may start in the workspace on forkspace's behalf.
        .args([
            "-c",
            "core.fsmonitor=false",
            "update-index",
            "-q",
            "--refresh",
        ])
        .current_dir(&workspace.view_path)
        .env(IDENTITY_ENV, &identity)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the hook runs between fork and exec and only calls setsid, which is
        // async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                // Its own process group, so `stop` can end it and anything it starts.
                libc::setsid();
                Ok(())
            });
        }
    }
    crate::process::background::lower(&mut command);
    let Ok(child) = command.spawn() else {
        return;
    };
    let pid = child.id();
    let identity = watch::spawned_identity(pid, identity);
    let path = record_path(&workspace.name);
    let recorded = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&path, format!("{pid}\n{identity}\n")));
    if recorded.is_err() {
        // Unrecorded, it would block `drop` while it runs: wait for it instead.
        let mut child = child;
        let _ = child.wait();
    }
}

/// The refresh of workspace `name`, if it is still running.
fn running(name: &str) -> Option<u32> {
    let recorded = fs::read_to_string(record_path(name)).ok()?;
    let mut lines = recorded.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let identity = lines.next().map(str::trim).filter(|line| !line.is_empty());
    watch::is_alive(pid, identity).then_some(pid)
}

/// The processes of the running refresh of `name`: not processes using the workspace.
pub fn pids(name: &str) -> Vec<u32> {
    running(name).into_iter().collect()
}

/// Stop the refresh of `name`, if it is running, and forget it. Used before the view is
/// removed: the index it may leave half written goes with the view.
pub fn stop(name: &str) {
    if let Some(pid) = running(name) {
        if let Ok(group) = libc::pid_t::try_from(pid) {
            // SAFETY: `kill` only signals; the group is the refresh's own session.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while watch::is_alive(pid, None) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let _ = fs::remove_file(record_path(name));
}
