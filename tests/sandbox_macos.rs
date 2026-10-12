//! The write sandbox on macOS (`sandbox-exec`): commands in a workspace cannot write to the
//! original project.

#![cfg(target_os = "macos")]

use std::fs;
use std::process::Command;

#[test]
fn seatbelt_refuses_writes_to_the_project_and_allows_the_view() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let home = root.path().join("home");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("app.txt"), "original\n").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", &home)
            .env("FORKSPACE_BACKEND", "copy")
            .env_remove("FORKSPACE_SANDBOX")
            .args(args)
            .output()
            .unwrap()
    };
    assert!(
        run(&["new", "ws", "--from", project.to_str().unwrap(), "--copy"])
            .status
            .success()
    );
    let script = format!(
        "echo agent > '{}'; echo inside > app.txt",
        project.join("app.txt").display()
    );
    run(&["enter", "ws", "--", "sh", "-c", &script]);
    assert_eq!(
        fs::read_to_string(project.join("app.txt")).unwrap(),
        "original\n"
    );
    assert_eq!(
        fs::read_to_string(home.join("workspaces/ws/app.txt")).unwrap(),
        "inside\n"
    );
    let _ = run(&["drop", "--all", "--kill"]);
}
