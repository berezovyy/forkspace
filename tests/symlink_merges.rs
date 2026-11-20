//! A base symlink that holds the target it had at fork is unchanged, however it got there, also
//! when the fork-time copy keeps no symlinks (the Linux copy fallback): `merge --undo` and
//! `git checkout` re-create links (OVL-06).

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use common::cli::{stderr, write, Env};
use serial_test::serial;

fn target(path: &Path) -> String {
    fs::read_link(path).unwrap().to_string_lossy().into_owned()
}

/// Fork `ws` with `c.md -> target-one`, then drop the link from the fork-time copy, as a copy
/// that keeps regular files only has it.
fn fork_with_link(env: &Env) -> PathBuf {
    symlink("target-one", env.project().join("c.md")).unwrap();
    write(&env.project().join("k"), "k\n");
    env.fork_copy("ws");
    let copy = forkspace::sync::ancestor::dir("ws").join("c.md");
    if fs::symlink_metadata(&copy).is_ok() {
        fs::remove_file(&copy).unwrap();
    }
    env.view("ws")
}

fn retarget(link: &Path, to: &str) {
    fs::remove_file(link).unwrap();
    symlink(to, link).unwrap();
}

#[test]
#[serial]
fn a_symlink_merge_undone_merges_again() {
    let env = Env::new();
    let view = fork_with_link(&env);
    retarget(&view.join("c.md"), "target-two");
    env.ok(&["merge", "ws"]);
    assert_eq!(target(&env.project().join("c.md")), "target-two");
    env.ok(&["undo", "ws"]);
    assert_eq!(target(&env.project().join("c.md")), "target-one");
    env.ok(&["merge", "ws"]);
    assert_eq!(target(&env.project().join("c.md")), "target-two");
}

#[test]
#[serial]
fn a_base_symlink_re_created_with_the_same_target_is_unchanged() {
    let env = Env::new();
    let view = fork_with_link(&env);
    std::thread::sleep(std::time::Duration::from_millis(20));
    retarget(&env.project().join("c.md"), "target-one");
    retarget(&view.join("c.md"), "target-two");
    env.ok(&["merge", "ws"]);
    assert_eq!(target(&env.project().join("c.md")), "target-two");
}

#[test]
#[serial]
fn a_base_symlink_retargeted_since_the_fork_is_a_conflict() {
    let env = Env::new();
    let view = fork_with_link(&env);
    std::thread::sleep(std::time::Duration::from_millis(20));
    retarget(&env.project().join("c.md"), "target-three");
    retarget(&view.join("c.md"), "target-two");
    let merged = env.run(&["merge", "ws"]);
    assert_eq!(merged.status.code(), Some(5), "{}", stderr(&merged));
    assert_eq!(target(&env.project().join("c.md")), "target-three");
}
