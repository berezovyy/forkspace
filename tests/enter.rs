mod common;

use std::path::PathBuf;
use std::process::Command;

use forkspace::state::State;
use serial_test::serial;

fn forkspace_bin() -> PathBuf {
    let mut path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    path.push("forkspace");
    path
}

#[test]
#[serial]
fn test_enter_command_passthrough() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws = common::fork_helper(base.path().to_path_buf(), "enter-cmd-ws").unwrap();

    let output = Command::new(forkspace_bin())
        .env("FORKSPACE_HOME", std::env::var("FORKSPACE_HOME").unwrap())
        .args(["enter", "enter-cmd-ws", "--", "echo", "hello"])
        .output()
        .expect("failed to run forkspace enter");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello"),
        "expected 'hello' in stdout, got: {}",
        stdout
    );
    assert!(
        output.status.success(),
        "forkspace enter failed: {:?}",
        output
    );

    common::cleanup_workspace(&ws);
}

#[test]
#[serial]
fn test_enter_env_vars() {
    skip_without_overlay!();
    let (_dir, base) = common::setup_with_project();

    let ws = common::fork_helper(base.path().to_path_buf(), "enter-env-ws").unwrap();

    let output = Command::new(forkspace_bin())
        .env("FORKSPACE_HOME", std::env::var("FORKSPACE_HOME").unwrap())
        .args([
            "enter",
            "enter-env-ws",
            "--",
            "sh",
            "-c",
            "printf '%s\\n%s\\n' \"$FORKSPACE_WORKSPACE\" \"$PWD\"",
        ])
        .output()
        .expect("failed to run forkspace enter");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("enter-env-ws"),
        "expected workspace name in stdout, got: {}",
        stdout
    );
    assert!(stdout.contains(&ws.view_path.display().to_string()));

    common::cleanup_workspace(&ws);
}

#[test]
#[serial]
fn enter_makes_a_missing_workspace_from_your_project() {
    let (_dir, base) = common::setup_with_project();
    let output = Command::new(forkspace_bin())
        .env("FORKSPACE_HOME", std::env::var("FORKSPACE_HOME").unwrap())
        .current_dir(base.path())
        .args(["enter", "new-enter-ws", "--", "echo", "inside-new"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("inside-new"));
    assert!(State::load()
        .unwrap()
        .workspaces
        .contains_key("new-enter-ws"));

    // With no workspace at all, `enter` makes one too.
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("a.txt"), "a\n").unwrap();
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(forkspace_bin())
        .env("FORKSPACE_HOME", home.path())
        .current_dir(other.path())
        .args(["enter", "--", "cat", "a.txt"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("created workspace"));
    common::unmount_under(home.path());
}

// ── Ephemeral tests ──

#[test]
#[serial]
fn test_ephemeral_flag_recorded_in_state() {
    let (_dir, base) = common::setup_with_project();

    let mut ws = forkspace::workspace::Workspace::new("eph-test", base.path().to_path_buf(), None);
    assert!(!ws.ephemeral);
    ws.ephemeral = true;

    let json = serde_json::to_string(&ws).unwrap();
    let deserialized: forkspace::workspace::Workspace = serde_json::from_str(&json).unwrap();
    assert!(deserialized.ephemeral);
}

#[test]
#[serial]
fn test_ephemeral_state_without_ephemeral_field() {
    let _dir = common::setup_test_env();
    let mut value = serde_json::to_value(forkspace::workspace::Workspace::new(
        "old-ws",
        "/tmp/base".into(),
        None,
    ))
    .unwrap();
    value.as_object_mut().unwrap().remove("ephemeral");
    let ws: forkspace::workspace::Workspace = serde_json::from_value(value).unwrap();
    assert!(!ws.ephemeral);
    assert!(!ws.tmpfs());
    assert_eq!(ws.tmpfs_size(), None);
}

/// `enter -- command` exits with the command's status, so a failing `make test` fails the
/// `enter`.
#[test]
#[serial]
fn enter_exits_with_the_command_status() {
    let (_dir, base) = common::setup_with_project();
    let home = std::env::var("FORKSPACE_HOME").unwrap();
    let forkspace = |args: &[&str]| {
        Command::new(forkspace_bin())
            .env("FORKSPACE_HOME", &home)
            .current_dir(base.path())
            .args(args)
            .output()
            .unwrap()
    };
    assert!(forkspace(&["new", "status-ws"]).status.success());

    let failed = forkspace(&["enter", "status-ws", "--", "sh", "-c", "exit 3"]);
    assert_eq!(failed.status.code(), Some(3), "{failed:?}");
    assert!(String::from_utf8_lossy(&failed.stderr).contains("exit code 3"));
    let killed = forkspace(&["enter", "status-ws", "--", "sh", "-c", "kill -TERM $$"]);
    assert_eq!(killed.status.code(), Some(128 + 15), "{killed:?}");
    assert!(forkspace(&["enter", "status-ws", "--", "true"])
        .status
        .success());

    assert!(forkspace(&["drop", "status-ws"]).status.success());
}
