//! `gc` never removes what a command is using: a workspace its own caller works in (GC-10).

mod common;

use std::process::Command;

use common::cli::{stdout, write, Env};
use forkspace::state::State;
use serial_test::serial;

/// An agent's shell works in an unchanged workspace and runs `gc` from elsewhere: `gc` is
/// that shell's child, so the process check that leaves out its own ancestors (for `drop`
/// from inside a workspace) must not make the workspace look unused.
#[test]
#[serial]
fn gc_keeps_the_workspace_its_own_caller_works_in() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork_copy("mine");
    env.fork_copy("other");
    let output = Command::new("sh")
        .current_dir(env.view("mine"))
        .env("FORKSPACE_HOME", env.home.path())
        .args([
            "-c",
            "(cd \"$1\" && exec \"$2\" gc --older-than 0s --yes)",
            "sh",
            env.outside().to_str().unwrap(),
            env!("CARGO_BIN_EXE_forkspace"),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = State::load().unwrap();
    assert!(state.workspaces.contains_key("mine"), "{}", stdout(&output));
    assert!(
        !state.workspaces.contains_key("other"),
        "{}",
        stdout(&output)
    );
    assert!(env.view("mine").is_dir());
}
