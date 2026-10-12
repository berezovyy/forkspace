//! One systemd user slice per workspace on Linux, so every process started through
//! `enter` or `run` stays findable and stoppable as a group.
//!
//! Commands run as `systemd-run --user --scope --slice=forkspace-<name>.slice --collect -- …`.
//! `--scope` keeps the caller's terminal, environment, and working directory, and every
//! descendant stays in the workspace's cgroup, including `nohup`, `setsid`, and double-forked
//! processes. The `-` in the slice name makes each workspace slice a child of
//! `forkspace.slice`, which carries limits for all workspaces together: a lower CPU weight than
//! the user's own apps and a shared memory ceiling, so parallel agents cannot freeze the
//! machine. A workspace's own `--memory` / `--cpu` limits go on its slice. Without a user
//! manager (containers, SSH sessions without lingering) commands run directly, unlimited, and
//! the `/proc` scan in [`super::members`] is the only way to find them.
//!
//! On macOS, where unprivileged users have no per-group limits, commands that are not an
//! interactive shell run at background priority (`PRIO_DARWIN_BG`, what `taskpolicy -b` sets):
//! lower CPU and disk priority than the user's apps.

use std::path::PathBuf;
use std::process::Command;

use crate::workspace::{Limits, Workspace};

/// Whether `systemd-run --user` can be used here.
pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::sync::OnceLock;
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            std::env::var("FORKSPACE_NO_SYSTEMD").is_err()
                && std::env::var_os("XDG_RUNTIME_DIR")
                    .is_some_and(|dir| PathBuf::from(dir).join("systemd/private").exists())
                && which("systemd-run")
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The slice unit of workspace `name`: `forkspace-<home>_<name>.slice`, where `<home>` tells
/// apart workspaces of the same name under different `FORKSPACE_HOME`s.
pub fn slice(name: &str) -> String {
    format!("forkspace-{:08x}_{}.slice", home_hash(), escape(name))
}

/// FNV-1a of the canonical `FORKSPACE_HOME`.
fn home_hash() -> u32 {
    let home = crate::paths::forkspace_home();
    let home = home.canonicalize().unwrap_or(home);
    home.as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0x811c_9dc5_u32, |hash, byte| {
            (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
        })
}

/// `command` set up to run in `workspace`: in its slice under systemd, at background
/// priority on macOS when `background` (anything but an interactive shell).
pub fn scoped(command: Command, workspace: &Workspace, background: bool) -> Command {
    if !available() {
        return lowered(command, background);
    }
    ensure_shared_limits();
    apply_limits(&workspace.name, &workspace.limits);
    let mut wrapped = Command::new("systemd-run");
    wrapped
        .args(["--user", "--scope", "--collect", "--quiet"])
        .arg(format!("--slice={}", slice(&workspace.name)))
        .arg("--")
        .arg(command.get_program())
        .args(command.get_args());
    if let Some(dir) = command.get_current_dir() {
        wrapped.current_dir(dir);
    }
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => wrapped.env(key, value),
            None => wrapped.env_remove(key),
        };
    }
    wrapped
}

#[cfg(target_os = "macos")]
fn lowered(mut command: Command, background: bool) -> Command {
    use std::os::unix::process::CommandExt;
    if background {
        unsafe {
            command.pre_exec(|| {
                // Best effort: an unchanged priority is not a reason to fail the command.
                libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
                Ok(())
            });
        }
    }
    command
}

#[cfg(not(target_os = "macos"))]
fn lowered(command: Command, _background: bool) -> Command {
    command
}

/// Set the limits shared by all workspaces on `forkspace.slice`, once per login session (the
/// properties are runtime-only, and the marker lives in `$XDG_RUNTIME_DIR`).
fn ensure_shared_limits() {
    let weight = std::env::var("FORKSPACE_CPU_WEIGHT").unwrap_or_else(|_| "50".to_string());
    let memory = std::env::var("FORKSPACE_MEMORY_HIGH").unwrap_or_else(|_| "75%".to_string());
    let properties = [
        format!("CPUWeight={weight}"),
        format!("MemoryHigh={memory}"),
        "TasksMax=25%".to_string(),
    ];
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let marker = PathBuf::from(runtime).join(format!(
        "forkspace-limits-{weight}-{}",
        memory.replace(['/', '%'], "_")
    ));
    if marker.exists() {
        return;
    }
    let applied = Command::new("systemctl")
        .args(["--user", "set-property", "--runtime", "forkspace.slice"])
        .args(&properties)
        .output()
        .is_ok_and(|output| output.status.success());
    if applied {
        let _ = std::fs::write(marker, "");
    }
}

/// Put `limits` on workspace `name`'s slice (runtime-only; reapplied at every start).
pub fn apply_limits(name: &str, limits: &Limits) {
    if !available() || limits.is_empty() {
        return;
    }
    let mut properties = Vec::new();
    if let Some(memory) = &limits.memory {
        properties.push(format!("MemoryHigh={memory}"));
    }
    if let Some(cpu) = &limits.cpu {
        properties.push(format!("CPUQuota={cpu}"));
    }
    set_properties(name, &properties);
}

/// Set unit `properties` on workspace `name`'s slice for this session.
pub fn set_properties(name: &str, properties: &[String]) {
    if !available() || properties.is_empty() {
        return;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "set-property", "--runtime", &slice(name)])
        .args(properties)
        .output();
}

/// Memory used by the processes in workspace `name`'s slice right now.
pub fn memory_current(name: &str) -> Option<u64> {
    let text = std::fs::read_to_string(slice_cgroup(name)?.join("memory.current")).ok()?;
    text.trim().parse().ok()
}

/// Every process in workspace `name`'s slice.
pub fn slice_pids(name: &str) -> Vec<u32> {
    let Some(root) = slice_cgroup(name) else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in walkdir::WalkDir::new(&root).into_iter().flatten() {
        if entry.file_name() == "cgroup.procs" {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                pids.extend(
                    text.lines()
                        .filter_map(|line| line.trim().parse::<u32>().ok()),
                );
            }
        }
    }
    pids
}

/// SIGKILL everything left in workspace `name`'s slice (cgroup kill, so nothing escapes).
pub fn kill_slice(name: &str) {
    if let Some(root) = slice_cgroup(name) {
        if std::fs::write(root.join("cgroup.kill"), "1").is_ok() {
            return;
        }
    }
    if available() {
        let _ = Command::new("systemctl")
            .args(["--user", "kill", "--signal=SIGKILL", &slice(name)])
            .output();
    }
}

/// Slices of this `FORKSPACE_HOME` whose workspace is not in `names` (dropped by a forkspace
/// that crashed, or removed by hand).
pub fn orphan_slices<'a>(names: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    if !available() {
        return Vec::new();
    }
    let known: std::collections::HashSet<String> =
        names.into_iter().map(|name| slice(name)).collect();
    let pattern = format!("forkspace-{:08x}_*", home_hash());
    let Ok(output) = Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            "--all",
            "--plain",
            "--no-legend",
            &pattern,
        ])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| unit.ends_with(".slice") && !known.contains(*unit))
        .map(str::to_string)
        .collect()
}

/// Stop the systemd unit `unit`.
pub fn stop_unit(unit: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", unit])
        .output();
}

/// Unload workspace `name`'s (now empty) slice.
pub fn stop_slice(name: &str) {
    if available() {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &slice(name)])
            .output();
    }
}

/// The cgroup directory of workspace `name`'s slice, if it exists.
fn slice_cgroup(name: &str) -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // Our own cgroup names the user manager: `0::/user.slice/user-1000.slice/user@1000.service/…`.
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = own.lines().find_map(|line| line.strip_prefix("0::"))?;
    let manager_end = path
        .find(".service/")
        .map(|index| index + ".service".len())?;
    let manager = &path[..manager_end];
    if !manager.contains("/user@") {
        return None;
    }
    let directory = PathBuf::from("/sys/fs/cgroup")
        .join(manager.trim_start_matches('/'))
        .join("forkspace.slice")
        .join(slice(name));
    directory.is_dir().then_some(directory)
}

/// `systemd-escape`: keep `[A-Za-z0-9:_.]` (no leading `.`), write other bytes as `\xNN`.
fn escape(name: &str) -> String {
    let mut escaped = String::new();
    for (index, byte) in name.bytes().enumerate() {
        let keep = byte.is_ascii_alphanumeric()
            || byte == b':'
            || byte == b'_'
            || (byte == b'.' && index > 0);
        if keep {
            escaped.push(char::from(byte));
        } else {
            escaped.push_str(&format!("\\x{byte:02x}"));
        }
    }
    escaped
}

#[cfg(target_os = "linux")]
fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn slice_names_escape_like_systemd() {
        assert_eq!(super::escape("agent-1"), "agent\\x2d1");
        assert_eq!(super::escape(".hidden ws"), "\\x2ehidden\\x20ws");
        let slice = super::slice("agent-1");
        assert!(slice.starts_with("forkspace-") && slice.ends_with("_agent\\x2d1.slice"));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    /// Commands started for `run` and `enter -- <command>` are in the Darwin background state.
    #[test]
    fn background_commands_run_at_background_priority() {
        let mut command = std::process::Command::new("sleep");
        command.arg("5");
        let mut child = super::lowered(command, true).spawn().unwrap();
        let pid = child.id() as libc::id_t;
        // XNU reports the process's background state (non-zero when backgrounded).
        let state = unsafe { libc::getpriority(libc::PRIO_DARWIN_PROCESS, pid) };
        let _ = child.kill();
        let _ = child.wait();
        assert!(state > 0, "getpriority(PRIO_DARWIN_PROCESS) = {state}");

        let mut command = std::process::Command::new("sleep");
        command.arg("5");
        let mut child = super::lowered(command, false).spawn().unwrap();
        let state =
            unsafe { libc::getpriority(libc::PRIO_DARWIN_PROCESS, child.id() as libc::id_t) };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(state, 0, "an interactive shell keeps normal priority");
    }
}
