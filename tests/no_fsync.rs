//! Overlay workspaces answer `fsync` without flushing (fuse-overlayfs `fsync=0`): a workspace
//! is scratch, and `merge` writes the project with full flushes.

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;

use common::cli::{stdout, write, Env};
use forkspace::backend::BackendState;
use serial_test::serial;

/// The options of the fuse-overlayfs daemon serving `merged`, if one does.
fn daemon_options(merged: &Path) -> Option<String> {
    let merged = merged.to_string_lossy().into_owned();
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path().join("cmdline")).ok())
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect::<Vec<_>>()
        })
        .find(|args| {
            args.first()
                .is_some_and(|program| program.ends_with("fuse-overlayfs"))
                && args.last() == Some(&merged)
        })
        .map(|args| args.join(" "))
}

#[test]
#[serial]
fn overlay_workspaces_mount_with_fsync_off() {
    skip_without_overlay!();
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.ok(&["new", "w"]);
    let BackendState::Overlay(layers) = env.workspace("w").backend_state else {
        panic!("expected an overlay workspace");
    };
    let options = daemon_options(&layers.merged).expect("the workspace's daemon");
    assert!(options.contains("fsync=0"), "{options}");
    write(&layers.merged.join("b.txt"), "b\n");
    assert!(stdout(&env.ok(&["diff", "w"])).contains("b.txt"));
}
