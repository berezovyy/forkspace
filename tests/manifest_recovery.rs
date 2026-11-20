//! A fork manifest never stays unusable while its baseline can be recovered: a background build
//! whose process died is restarted (STATE-02), a lost or damaged manifest is built again from the
//! view, the base and the fork-time copy (STATE-03, STATE-11), and a manifest an older forkspace
//! marked degraded is used again when it is intact (UPG-01).

mod common;

use std::fs;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

use common::cli::{stderr, stdout, write, Env};
use forkspace::changes::manifest::{self, Builder, ManifestStatus};
use forkspace::state::State;
use serial_test::serial;

fn status(name: &str) -> ManifestStatus {
    State::load().unwrap().workspaces[name].manifest_status
}

/// Wait until the manifest of `name` is no longer `building`, and return its status.
#[cfg(target_os = "macos")]
fn settled_status(name: &str) -> ManifestStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = status(name);
        if status != ManifestStatus::Building || Instant::now() > deadline {
            return status;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The PID of a process that has exited.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    child.wait().unwrap();
    child.id()
}

/// Record `status` for `name` and forget its manifest file, as a crash or a lost file leaves it.
fn lose_manifest(name: &str, status: ManifestStatus, builder: Option<Builder>) {
    State::with_state(|state| {
        let workspace = state.require_workspace_mut(name)?;
        workspace.manifest_status = status;
        workspace.manifest_builder = builder;
        Ok(())
    })
    .unwrap();
    fs::remove_file(manifest::manifest_path(name)).unwrap();
}

/// A script that starts like the manifest builder and exits at once, as a builder killed
/// before it built anything.
#[cfg(target_os = "macos")]
fn dying_builder(env: &Env) -> PathBuf {
    let path = env.root.path().join("dying-builder");
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    path
}

/// Fork the project as workspace `ws` with `builder` started as its background builder.
#[cfg(target_os = "macos")]
fn fork_with_builder(env: &Env, builder: PathBuf) {
    manifest::set_builder_executable(builder);
    let forked = forkspace::workspace::create(env.project(), "ws", Default::default());
    common::use_forkspace_for_workers();
    forked.unwrap();
}

/// STATE-02: the builder dies (killed, crashed, or it gave up waiting for the workspace record)
/// and the status stays `building`; the next command, of any kind, starts it again.
#[cfg(target_os = "macos")]
#[test]
#[serial]
fn a_manifest_build_whose_builder_died_is_restarted_by_the_next_command() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    write(&env.project().join("b.txt"), "b\n");
    let project = env.project();
    fork_with_builder(&env, dying_builder(&env));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(status("ws"), ManifestStatus::Building);
    let view = env.view("ws");
    write(&view.join("a.txt"), "a edited\n");
    fs::remove_file(view.join("b.txt")).unwrap();

    env.ok(&["ls"]);
    assert_eq!(settled_status("ws"), ManifestStatus::Ready);
    let files = stdout(&env.ok(&["diff", "ws", "--files"]));
    assert!(
        files.contains("a.txt") && files.contains("b.txt"),
        "{files}"
    );
    env.ok(&["merge", "ws", "--no-install"]);
    assert_eq!(
        fs::read_to_string(project.join("a.txt")).unwrap(),
        "a edited\n"
    );
    assert!(!project.join("b.txt").exists());
}

/// STATE-02 where no background builder can run (Linux, or a builder that cannot start): a
/// command that needs the manifest builds it itself instead of refusing for good.
#[test]
#[serial]
fn a_command_that_needs_the_manifest_builds_it_when_its_builder_is_gone() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    write(&env.project().join("b.txt"), "b\n");
    env.fork_copy("ws");
    let view = env.view("ws");
    std::thread::sleep(Duration::from_millis(20));
    write(&view.join("a.txt"), "a edited\n");
    fs::remove_file(view.join("b.txt")).unwrap();
    lose_manifest(
        "ws",
        ManifestStatus::Building,
        Some(Builder::of(dead_pid())),
    );

    // This process stands for a platform without background builders.
    let _no_builders = NoBackgroundBuilders::new();
    manifest::settle_now("ws", true).unwrap();
    assert_eq!(status("ws"), ManifestStatus::Ready);
    let files = stdout(&env.ok(&["diff", "ws", "--files"]));
    assert!(
        files.contains("a.txt") && files.contains("b.txt"),
        "{files}"
    );
    env.ok(&["merge", "ws", "--no-install"]);
    assert_eq!(
        fs::read_to_string(env.project().join("a.txt")).unwrap(),
        "a edited\n"
    );
    assert!(!env.project().join("b.txt").exists());
}

/// Background builders of this process cannot start: their binary does not exist.
struct NoBackgroundBuilders;

impl NoBackgroundBuilders {
    fn new() -> Self {
        manifest::set_builder_executable("/nonexistent/forkspace-builder".into());
        Self
    }
}

impl Drop for NoBackgroundBuilders {
    fn drop(&mut self) {
        common::use_forkspace_for_workers();
    }
}

/// STATE-11: a workspace write and a base rename of a dangling symlink, both before the
/// background walk, used to fail the build (the walk followed the lost symlink) and leave the
/// workspace degraded for good.
#[cfg(target_os = "macos")]
#[test]
#[serial]
fn a_base_rename_of_a_dangling_symlink_before_the_walk_leaves_the_manifest_ready() {
    let env = Env::new();
    fs::create_dir(env.project().join("g.md")).unwrap();
    std::os::unix::fs::symlink("../missing", env.project().join("g.md/b")).unwrap();
    let slow_builder = env.root.path().join("slow-builder");
    fs::write(
        &slow_builder,
        format!(
            "#!/bin/sh\nsleep 1\nexec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_forkspace")
        ),
    )
    .unwrap();
    fs::set_permissions(
        &slow_builder,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let project = env.project();
    fork_with_builder(&env, slow_builder);
    write(&env.view("ws").join("b.txt"), "eps\n");
    fs::rename(project.join("g.md/b"), project.join("h")).unwrap();

    assert_eq!(settled_status("ws"), ManifestStatus::Ready);
    env.ok(&["merge", "ws", "--no-install"]);
    assert_eq!(fs::read_to_string(project.join("b.txt")).unwrap(), "eps\n");
    assert!(fs::symlink_metadata(project.join("h")).is_ok());
}

/// STATE-03: a manifest lost after the workspace was written to is built again, and a file the
/// workspace deleted that the base also changed since the fork comes back from the fork-time
/// copy, so the deletion is still seen and the merge reports the conflict.
#[test]
#[serial]
fn a_lost_manifest_is_rebuilt_and_keeps_deletions_of_files_the_base_also_changed() {
    let env = Env::new();
    write(&env.project().join("x.txt"), "x\n");
    write(&env.project().join("y.txt"), "y\n");
    write(&env.project().join("keep.txt"), "keep\n");
    env.fork_copy("ws");
    let view = env.view("ws");
    std::thread::sleep(Duration::from_millis(20));
    fs::remove_file(view.join("x.txt")).unwrap();
    write(&view.join("y.txt"), "y edited\n");
    write(&env.project().join("x.txt"), "x edited in the base\n");
    lose_manifest("ws", ManifestStatus::Degraded, None);

    manifest::settle_now("ws", true).unwrap();
    assert_eq!(status("ws"), ManifestStatus::Ready);
    let files = stdout(&env.ok(&["diff", "ws", "--files"]));
    assert!(
        files.contains("x.txt") && files.contains("y.txt"),
        "{files}"
    );
    assert!(!files.contains("keep.txt"), "{files}");
    let merged = env.run(&["merge", "ws", "--no-install"]);
    assert_eq!(merged.status.code(), Some(5), "{}", stderr(&merged));
    assert!(stderr(&merged).contains("x.txt"), "{}", stderr(&merged));
    assert_eq!(
        fs::read_to_string(env.project().join("x.txt")).unwrap(),
        "x edited in the base\n"
    );
}

/// After a pull the manifest describes the pulled base: a rebuild must not take the view's
/// entries for the baseline, or the workspace's own edits made before the pull would vanish
/// from `diff` and from the merge.
#[test]
#[serial]
fn a_pulled_workspace_rebuilds_its_manifest_from_the_pull_not_the_view() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    write(&env.project().join("b.txt"), "b\n");
    env.fork_copy("ws");
    let view = env.view("ws");
    std::thread::sleep(Duration::from_millis(20));
    write(&view.join("a.txt"), "a edited\n");
    write(&env.project().join("c.txt"), "c from the base\n");
    env.ok(&["pull", "ws", "--no-install"]);
    assert_eq!(stdout(&env.ok(&["diff", "ws", "--files"])).trim(), "a.txt");

    lose_manifest("ws", ManifestStatus::Degraded, None);
    manifest::settle_now("ws", true).unwrap();
    assert_eq!(status("ws"), ManifestStatus::Ready);
    assert_eq!(stdout(&env.ok(&["diff", "ws", "--files"])).trim(), "a.txt");
}

/// UPG-01: an older forkspace that cannot read the manifest marks it degraded; the manifest
/// itself is intact and is used again as it is.
#[test]
#[serial]
fn an_intact_manifest_marked_degraded_is_used_again() {
    let env = Env::new();
    write(&env.project().join("a.txt"), "a\n");
    env.fork_copy("ws");
    let before = fs::read(manifest::manifest_path("ws")).unwrap();
    State::with_state(|state| {
        state.require_workspace_mut("ws")?.manifest_status = ManifestStatus::Degraded;
        Ok(())
    })
    .unwrap();

    write(&env.view("ws").join("a.txt"), "a edited\n");
    env.ok(&["merge", "ws", "--no-install"]);
    assert_eq!(fs::read(manifest::manifest_path("ws")).ok(), Some(before));
    assert_eq!(
        fs::read_to_string(env.project().join("a.txt")).unwrap(),
        "a edited\n"
    );
}
