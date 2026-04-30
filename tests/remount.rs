mod common;

use std::fs;
use std::process::Command;

use forkspace::backend::overlay::mount::unmount_overlay;
use serial_test::serial;

fn forkspace(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(args)
        .env(
            "FORKSPACE_HOME",
            std::env::var_os("FORKSPACE_HOME").expect("FORKSPACE_HOME is set by the test"),
        )
        .output()
        .unwrap()
}

#[test]
#[serial]
fn enter_remounts_an_overlay_view_that_was_unmounted() {
    skip_without_overlay!();
    let (_home, base) = common::setup_with_project();
    let ws = common::fork_helper(base.path().to_path_buf(), "remount-ws").unwrap();
    fs::write(ws.view_path.join("note.txt"), "kept in upper\n").unwrap();

    // What a reboot leaves behind: the layers on disk and an empty, unmounted mount point.
    unmount_overlay(&ws.view_path).unwrap();
    assert!(!ws.view_path.join("README.md").exists());

    let output = forkspace(&["enter", "remount-ws", "--", "cat", "note.txt", "README.md"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("kept in upper"), "{stdout}");
    assert!(stdout.contains("# Test Project"), "{stdout}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("remounted workspace 'remount-ws'"));

    common::cleanup_workspace(&ws);
}

#[test]
#[serial]
fn enter_refuses_a_mount_point_holding_stray_files() {
    skip_without_overlay!();
    let (_home, base) = common::setup_with_project();
    let ws = common::fork_helper(base.path().to_path_buf(), "stray-ws").unwrap();
    unmount_overlay(&ws.view_path).unwrap();
    fs::write(ws.view_path.join("lost.txt"), "written while unmounted\n").unwrap();

    let output = forkspace(&["enter", "stray-ws", "--", "true"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("written while it was unmounted"),
        "{stderr}"
    );
    assert!(ws.view_path.join("lost.txt").exists());

    fs::remove_file(ws.view_path.join("lost.txt")).unwrap();
    common::cleanup_workspace(&ws);
}

#[test]
#[serial]
fn tmpfs_workspace_whose_layers_vanished_reports_the_reboot() {
    skip_without_overlay!();
    let (_home, base) = common::setup_with_project();
    let ws = forkspace::workspace::create(
        base.path().to_path_buf(),
        "lost-tmpfs-ws",
        forkspace::workspace::CreateOptions {
            tmpfs: true,
            ..Default::default()
        },
    )
    .unwrap();
    unmount_overlay(&ws.view_path).unwrap();
    fs::remove_dir_all(ws.upper_path().unwrap().parent().unwrap()).unwrap();

    let output = forkspace(&["enter", "lost-tmpfs-ws", "--", "true"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("lost at reboot"), "{stderr}");

    let output = forkspace(&["drop", "lost-tmpfs-ws"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
