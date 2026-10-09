//! Processes running in a workspace: found, listed, and stopped on `drop --kill`.
//! The checks read `/proc`, so this suite runs on Linux.

#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Env {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
    backend: &'static str,
    systemd: bool,
}

impl Env {
    fn new(backend: &'static str, systemd: bool) -> Self {
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
            backend,
            systemd,
        };
        let project = env.root.path().join("project");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "a\n").unwrap();
        let mut args = vec!["new", "ws", "--from", project.to_str().unwrap()];
        if backend == "copy" {
            args.push("--copy");
        }
        assert!(env.run(&args).status.success());
        env
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forkspace"));
        command
            .env("FORKSPACE_HOME", self.home.path())
            .env("FORKSPACE_BACKEND", self.backend)
            .env("NO_COLOR", "1")
            .args(args);
        if !self.systemd {
            command.env("FORKSPACE_NO_SYSTEMD", "1");
        }
        command.output().unwrap()
    }

    /// Start `script` in the background through `enter`, the way an agent would.
    fn start(&self, script: &str) {
        let output = self.run(&["enter", "ws", "--", "sh", "-c", script]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["drop", "--all", "--kill"]);
    }
}

/// Processes whose command line is exactly `sleep <marker>`.
fn running(marker: &str) -> Vec<u32> {
    let wanted = format!("sleep {marker}");
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let command = fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let command = String::from_utf8_lossy(&command).replace('\0', " ");
        let state = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
        let zombie = state
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
        if command.trim() == wanted && !zombie {
            pids.push(pid);
        }
    }
    pids
}

fn wait_until_gone(marker: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if running(marker).is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn systemd_user_available() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|dir| PathBuf::from(dir).join("systemd/private").exists())
}

fn a_background_process_blocks_drop_until_killed(backend: &'static str, marker: &str) {
    let env = Env::new(backend, true);
    env.start(&format!("nohup sleep {marker} >/dev/null 2>&1 & exit 0"));
    assert_eq!(running(marker).len(), 1);

    let listed = String::from_utf8(env.run(&["ls"]).stdout).unwrap();
    assert!(listed.contains("1 process"), "{listed}");

    let refused = env.run(&["drop", "ws"]);
    assert_eq!(refused.status.code(), Some(5));
    let message = String::from_utf8_lossy(&refused.stderr);
    let pid = running(marker)[0];
    assert!(message.contains(&format!("sleep (pid {pid}")), "{message}");
    assert!(message.contains("--kill"), "{message}");

    let killed = env.run(&["drop", "ws", "--kill"]);
    assert!(
        killed.status.success(),
        "{}",
        String::from_utf8_lossy(&killed.stderr)
    );
    assert!(
        wait_until_gone(marker),
        "sleep {marker} survived drop --kill"
    );
}

#[test]
fn copy_workspace_processes_block_drop_until_killed() {
    a_background_process_blocks_drop_until_killed("copy", "3101");
}

#[test]
fn overlay_workspace_processes_block_drop_until_killed() {
    skip_without_overlay!();
    a_background_process_blocks_drop_until_killed("linux", "3102");
}

#[test]
fn processes_that_leave_the_workspace_directory_are_still_killed_through_the_slice() {
    if !systemd_user_available() {
        eprintln!("skipping: no systemd user manager");
        return;
    }
    let env = Env::new("copy", true);
    env.start("setsid sh -c 'cd / && exec sleep 3103' >/dev/null 2>&1 & exit 0");
    assert_eq!(running("3103").len(), 1);
    let refused = env.run(&["drop", "ws"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("sleep"));
    assert!(env.run(&["drop", "ws", "--kill"]).status.success());
    assert!(wait_until_gone("3103"));
}

#[test]
fn without_systemd_the_proc_scan_still_finds_the_process() {
    let env = Env::new("copy", false);
    env.start("nohup sleep 3104 >/dev/null 2>&1 & exit 0");
    let refused = env.run(&["drop", "ws"]);
    assert_eq!(refused.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("sleep"));
    assert!(env.run(&["drop", "ws", "--kill"]).status.success());
    assert!(wait_until_gone("3104"));
    assert!(!env.home.path().join("workspaces/ws").exists());
}

/// The cgroup directory of the slice whose name ends in `_<workspace>.slice`.
fn slice_cgroup(workspace: &str) -> Option<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let parent = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/forkspace.slice"
    ));
    fs::read_dir(&parent)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&format!("_{workspace}.slice")))
                && has_procs(path)
        })
}

fn has_procs(path: &std::path::Path) -> bool {
    walk_procs(path).next().is_some()
}

fn walk_procs(path: &std::path::Path) -> impl Iterator<Item = u32> {
    let mut pids = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        pids.extend(
            fs::read_to_string(dir.join("cgroup.procs"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.parse::<u32>().ok()),
        );
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                stack.push(entry.path());
            }
        }
    }
    pids.into_iter()
}

#[test]
fn workspace_processes_run_under_forkspace_slice_with_limits() {
    if !systemd_user_available() {
        eprintln!("skipping: no systemd user manager");
        return;
    }
    let env = Env::new("copy", true);
    // A distinct workspace name, so the slice can be told apart from parallel tests'.
    let project = env.root.path().join("project");
    let created = env.run(&[
        "new",
        "limited",
        "--from",
        project.to_str().unwrap(),
        "--copy",
        "--memory",
        "1G",
        "--cpu",
        "150%",
    ]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let started = env.run(&[
        "enter",
        "limited",
        "--",
        "sh",
        "-c",
        "nohup sleep 3105 >/dev/null 2>&1 & exit 0",
    ]);
    assert!(started.status.success());
    std::thread::sleep(Duration::from_millis(300));

    let slice = slice_cgroup("limited").expect("workspace slice with its process");
    let pid = running("3105")[0];
    assert!(walk_procs(&slice).any(|member| member == pid));
    assert_eq!(
        fs::read_to_string(slice.join("memory.high"))
            .unwrap()
            .trim(),
        "1073741824"
    );
    assert_eq!(
        fs::read_to_string(slice.join("cpu.max")).unwrap().trim(),
        "150000 100000"
    );
    let parent = slice.parent().unwrap();
    assert_eq!(parent.file_name().unwrap(), "forkspace.slice");
    assert_eq!(
        fs::read_to_string(parent.join("cpu.weight"))
            .unwrap()
            .trim(),
        "50"
    );
    assert_ne!(
        fs::read_to_string(parent.join("memory.high"))
            .unwrap()
            .trim(),
        "max"
    );

    let listed: serde_json::Value =
        serde_json::from_slice(&env.run(&["ls", "--json"]).stdout).unwrap();
    let entry = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "limited")
        .unwrap();
    assert!(entry["memory_bytes"].as_u64().unwrap() > 0);
    assert_eq!(entry["limits"]["memory"], "1G");

    let changed = env.run(&["limit", "limited", "--memory", "2G"]);
    assert!(changed.status.success());
    assert_eq!(
        fs::read_to_string(slice.join("memory.high"))
            .unwrap()
            .trim(),
        "2147483648"
    );
    assert_eq!(
        env.run(&["limit", "limited", "--cpu", "lots"])
            .status
            .code(),
        Some(1)
    );

    assert!(env.run(&["drop", "limited", "--kill"]).status.success());
    assert!(wait_until_gone("3105"));
}
