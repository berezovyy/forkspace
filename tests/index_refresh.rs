//! A new workspace's Git index is refreshed in the background (`src/workspace/git_index.rs`),
//! so the first `git status` in it does not hash every tracked file again.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::cli::{write, Env};
use serial_test::serial;

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap()
}

fn repository(env: &Env) {
    for index in 0..200 {
        write(
            &env.project().join(format!("d{}/f{index}.txt", index % 10)),
            "x\n",
        );
    }
    assert!(git(&env.project(), &["init", "-q"]).status.success());
    assert!(git(&env.project(), &["add", "-A"]).status.success());
    assert!(git(&env.project(), &["commit", "-qm", "init"])
        .status
        .success());
}

/// Whether the index's stat data matches every tracked file: `diff-files` compares stat data
/// only, without reading contents.
fn index_is_fresh(view: &std::path::Path) -> bool {
    git(view, &["diff-files", "--quiet"]).status.success()
}

#[test]
#[serial]
fn a_new_workspace_gets_a_refreshed_index_without_a_status() {
    let env = Env::new();
    repository(&env);
    env.fork_copy("w");
    let view = env.view("w");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !index_is_fresh(&view) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(index_is_fresh(&view), "the index was never refreshed");
    // Only the index changed: no file of the workspace is a change.
    let diff = env.ok(&["diff", "w", "--files"]);
    let diff = String::from_utf8_lossy(&diff.stdout);
    assert!(diff.trim().is_empty() || diff.contains("clean"), "{diff}");
}

#[test]
#[serial]
fn the_refresh_can_be_turned_off() {
    let env = Env::new();
    repository(&env);
    let project = env.project();
    let output = env
        .command(&["new", "w", "--from", project.to_str().unwrap(), "--copy"])
        .env("FORKSPACE_GIT_REFRESH", "0")
        .output()
        .unwrap();
    assert!(output.status.success());
    std::thread::sleep(Duration::from_millis(300));
    assert!(!index_is_fresh(&env.view("w")));
}

#[test]
#[serial]
fn a_running_refresh_neither_blocks_drop_nor_counts_as_a_process() {
    let env = Env::new();
    repository(&env);
    for round in 0..5 {
        let name = format!("w{round}");
        env.fork_copy(&name);
        let listed = env.ok(&["ls", "--json"]);
        let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
        let processes = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|workspace| workspace["name"] == name.as_str())
            .unwrap()["processes"]
            .as_u64()
            .unwrap();
        assert_eq!(processes, 0);
        env.ok(&["drop", &name]);
    }
}
