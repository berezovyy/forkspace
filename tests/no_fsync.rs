//! `--no-fsync`: an overlay workspace mounted with fuse-overlayfs `fsync=0`, recorded so that
//! a remount keeps it, and implied by `--tmpfs` and `enter --ephemeral`.

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

fn overlay(env: &Env, name: &str) -> Option<forkspace::backend::OverlayState> {
    match env.workspace(name).backend_state {
        BackendState::Overlay(layers) => Some(layers),
        _ => None,
    }
}

#[test]
#[serial]
fn no_fsync_mounts_with_fsync_off_and_default_workspaces_do_not() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    let from = env.project();
    let from = from.to_str().unwrap();
    env.ok(&["new", "durable", "--from", from]);
    let Some(durable) = overlay(&env, "durable") else {
        eprintln!("FORKSPACE_TEST_SKIP {{\"capability\":\"fuse-overlayfs\",\"reason\":\"no overlay backend\"}}");
        return;
    };
    env.ok(&["new", "fast", "--from", from, "--no-fsync"]);
    let fast = overlay(&env, "fast").unwrap();

    assert!(!durable.no_fsync);
    assert!(fast.no_fsync);
    let durable_options = daemon_options(&durable.merged).expect("daemon of 'durable'");
    let fast_options = daemon_options(&fast.merged).expect("daemon of 'fast'");
    assert!(!durable_options.contains("fsync=0"), "{durable_options}");
    assert!(fast_options.contains("fsync=0"), "{fast_options}");
    // The workspace works as usual.
    write(&fast.merged.join("b.txt"), "b\n");
    assert!(stdout(&env.ok(&["diff", "fast", "--files"])).contains("b.txt"));
}

#[test]
#[serial]
fn ephemeral_workspaces_do_not_flush_on_fsync() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    let from = env.project();
    // The command reports the options of the daemon serving its own workspace.
    let output = env.ok(&[
        "enter",
        "--ephemeral",
        "--from",
        from.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        "for pid in $(pgrep -x fuse-overlayfs); do tr '\\0' ' ' < /proc/$pid/cmdline; echo; done | grep -F \"$FORKSPACE_VIEW\" || true",
    ]);
    let printed = stdout(&output);
    if std::env::var_os("FORKSPACE_TEST_REQUIRE_OVERLAY").is_some() {
        assert!(
            printed.contains("lowerdir="),
            "no overlay daemon found: {printed}"
        );
    }
    if printed.contains("lowerdir=") {
        assert!(printed.contains("fsync=0"), "{printed}");
    }
}
