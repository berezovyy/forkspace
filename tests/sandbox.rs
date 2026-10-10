//! The write sandbox (Landlock on Linux): commands in a workspace cannot write to the original
//! project or to other workspaces.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

/// Project, FORKSPACE_HOME, and a fake HOME live under the build directory rather than `/tmp`,
/// which the sandbox leaves writable.
struct Env {
    root: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let env = Self { root };
        for directory in ["project", "home", "fake-home/.cache"] {
            fs::create_dir_all(env.path(directory)).unwrap();
        }
        fs::write(env.path("project/app.txt"), "original\n").unwrap();
        for name in ["ws", "other"] {
            let project = env.path("project");
            let output = env.run(
                &["new", name, "--from", project.to_str().unwrap(), "--copy"],
                &[],
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        env
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn view(&self, name: &str) -> PathBuf {
        self.path("home/workspaces").join(name)
    }

    fn run(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forkspace"));
        command
            .env("FORKSPACE_HOME", self.path("home"))
            .env("FORKSPACE_BACKEND", "copy")
            .env("FORKSPACE_NO_SYSTEMD", "1")
            .env("HOME", self.path("fake-home"))
            .env_remove("FORKSPACE_SANDBOX")
            .env_remove("FORKSPACE_NO_LANDLOCK")
            .args(args);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    /// Run `script` with `sh -c` inside workspace `ws`.
    fn sh(&self, script: &str, extra_args: &[&str]) -> Output {
        let mut args = vec!["enter", "ws"];
        args.extend_from_slice(extra_args);
        args.extend_from_slice(&["--", "sh", "-c", script]);
        self.run(&args, &[])
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["drop", "--all", "--kill"], &[]);
    }
}

fn landlock_available() -> bool {
    // ABI version query: landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION).
    let abi = unsafe { libc::syscall(444, std::ptr::null::<u8>(), 0_usize, 1_u32) };
    abi >= 1
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn writes_to_the_project_are_refused_and_writes_inside_succeed() {
    if !landlock_available() {
        eprintln!("skipping: no Landlock on this kernel");
        return;
    }
    let env = Env::new();
    let project_file = env.path("project/app.txt");
    let script = format!(
        "echo agent > '{}'; echo inside > app.txt; echo scratch > /tmp/forkspace-sandbox-$$ && rm /tmp/forkspace-sandbox-$$; echo cached > \"$HOME/.cache/entry\"",
        project_file.display()
    );
    let output = env.sh(&script, &[]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Permission denied"));
    assert_eq!(read(&project_file), "original\n");
    assert_eq!(read(&env.view("ws").join("app.txt")), "inside\n");
    assert_eq!(read(&env.path("fake-home/.cache/entry")), "cached\n");
}

#[test]
fn another_workspace_is_not_writable() {
    if !landlock_available() {
        return;
    }
    let env = Env::new();
    let other = env.view("other").join("app.txt");
    env.sh(&format!("echo intruder > '{}'", other.display()), &[]);
    assert_eq!(read(&other), "original\n");
}

#[test]
fn background_children_are_restricted_too() {
    if !landlock_available() {
        return;
    }
    let env = Env::new();
    let project_file = env.path("project/app.txt");
    let script = format!(
        "(sleep 0.2; echo late > '{}') >/dev/null 2>&1 & exit 0",
        project_file.display()
    );
    env.sh(&script, &[]);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(read(&project_file), "original\n");
}

#[test]
fn no_sandbox_restores_unrestricted_writes() {
    let env = Env::new();
    let project_file = env.path("project/app.txt");
    env.sh(
        &format!("echo agent > '{}'", project_file.display()),
        &["--no-sandbox"],
    );
    assert_eq!(read(&project_file), "agent\n");
}

#[test]
fn without_landlock_the_command_runs_with_one_warning() {
    let env = Env::new();
    let output = env.run(
        &["enter", "ws", "--", "sh", "-c", "echo ran > app.txt"],
        &[("FORKSPACE_NO_LANDLOCK", "1")],
    );
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("write sandbox is off").count(),
        1,
        "{stderr}"
    );
    assert_eq!(read(&env.view("ws").join("app.txt")), "ran\n");
}
