//! One systemd user slice per workspace on Linux, so every process started through
//! `enter` or `run` stays findable and stoppable as a group.
//!
//! Commands run as `systemd-run --user --scope --slice=forkspace-<name>.slice --collect -- …`.
//! `--scope` keeps the caller's terminal, environment, and working directory, and every
//! descendant stays in the workspace's cgroup, including `nohup`, `setsid`, and double-forked
//! processes. The `-` in the slice name makes each workspace slice a child of
//! `forkspace.slice`, where resource limits for all workspaces can be set. Without a user
//! manager (containers, SSH sessions without lingering) commands run directly and the
//! `/proc` scan in [`super::members`] is the only way to find them.

use std::path::PathBuf;
use std::process::Command;

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

/// `command` wrapped to run in workspace `name`'s slice, or unchanged without systemd.
pub fn scoped(command: Command, name: &str) -> Command {
    if !available() {
        return command;
    }
    let mut wrapped = Command::new("systemd-run");
    wrapped
        .args(["--user", "--scope", "--collect", "--quiet"])
        .arg(format!("--slice={}", slice(name)))
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
