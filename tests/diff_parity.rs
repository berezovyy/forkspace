mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use forkspace::backend::{BackendKind, BackendState};
use forkspace::changes::filter::ChangeClass;
use forkspace::changes::manifest::{self, ManifestStatus};
use forkspace::changes::{collect_clone_changes, generate_unified_diff, ChangeKind, OldContent};
use forkspace::util::now_unix_ns;
use forkspace::workspace::Workspace;
use serial_test::serial;

fn clone_workspace(base: &Path, view: &Path, name: &str) -> Workspace {
    let mut workspace = Workspace::new(name, base.to_path_buf(), None);
    workspace.backend_kind = BackendKind::MacClone;
    workspace.backend_state = BackendState::MacClone {
        unknown_fields: serde_json::Map::new(),
    };
    workspace.view_path = view.to_path_buf();
    std::thread::sleep(std::time::Duration::from_millis(2));
    workspace.fork_time_ns = now_unix_ns();
    workspace.manifest_status = ManifestStatus::Ready;
    manifest::build(name, view, workspace.fork_time_ns).unwrap();
    workspace
}

fn write_pair(base: &Path, view: &Path, relative: &str, content: &[u8]) {
    let base_path = base.join(relative);
    let view_path = view.join(relative);
    fs::create_dir_all(base_path.parent().unwrap()).unwrap();
    fs::create_dir_all(view_path.parent().unwrap()).unwrap();
    fs::write(&base_path, content).unwrap();
    fs::write(&view_path, content).unwrap();
    let modified = fs::metadata(&view_path).unwrap().modified().unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&base_path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
}

fn kind(changes: &[forkspace::changes::FileChange], path: &str) -> ChangeKind {
    changes
        .iter()
        .find(|change| change.path == Path::new(path))
        .unwrap_or_else(|| panic!("missing change for {path}: {changes:?}"))
        .kind
}

#[test]
#[serial]
fn clone_scan_classifies_mutations_deletions_and_metadata() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "modified", b"old\n");
    write_pair(base.path(), view.path(), "deleted", b"gone\n");
    write_pair(base.path(), view.path(), "mode", b"same\n");
    write_pair(base.path(), view.path(), "tree/a", b"a\n");
    write_pair(base.path(), view.path(), "tree/b", b"b\n");
    write_pair(base.path(), view.path(), "recreated", b"before\n");
    std::os::unix::fs::symlink("modified", base.path().join("link")).unwrap();
    std::os::unix::fs::symlink("modified", view.path().join("link")).unwrap();
    let workspace = clone_workspace(base.path(), view.path(), "scan");

    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(view.path().join("modified"), b"new\n").unwrap();
    fs::write(view.path().join("added"), b"new\n").unwrap();
    fs::remove_file(view.path().join("deleted")).unwrap();
    fs::remove_dir_all(view.path().join("tree")).unwrap();
    fs::remove_file(view.path().join("recreated")).unwrap();
    fs::write(view.path().join("recreated"), b"after\n").unwrap();
    let mut permissions = fs::metadata(view.path().join("mode"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(view.path().join("mode"), permissions).unwrap();
    fs::remove_file(view.path().join("link")).unwrap();
    std::os::unix::fs::symlink("added", view.path().join("link")).unwrap();

    let changes = collect_clone_changes(&workspace, true, true).unwrap();
    assert!(matches!(kind(&changes, "added"), ChangeKind::Added));
    assert!(matches!(kind(&changes, "modified"), ChangeKind::Modified));
    assert!(matches!(kind(&changes, "deleted"), ChangeKind::Deleted));
    assert!(matches!(kind(&changes, "mode"), ChangeKind::Modified));
    assert!(matches!(kind(&changes, "recreated"), ChangeKind::Modified));
    assert!(matches!(kind(&changes, "link"), ChangeKind::Modified));
    assert!(matches!(kind(&changes, "tree"), ChangeKind::Deleted));
    assert!(!changes
        .iter()
        .any(|change| change.path == Path::new("tree/a")));
    let mode = changes
        .iter()
        .find(|change| change.path == Path::new("mode"))
        .unwrap();
    assert_eq!((mode.additions, mode.deletions), (Some(0), Some(0)));
}

#[test]
#[serial]
fn clone_scan_handles_base_advance_filters_and_pending_manifest() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "advanced", b"old\n");
    write_pair(base.path(), view.path(), "Cargo.lock", b"old\n");
    write_pair(base.path(), view.path(), ".git/index", b"index\n");
    write_pair(base.path(), view.path(), ".DS_Store", b"finder\n");
    fs::write(base.path().join(".gitignore"), b"Cargo.lock\nignored/\n").unwrap();
    fs::write(view.path().join(".gitignore"), b"Cargo.lock\nignored/\n").unwrap();
    write_pair(base.path(), view.path(), "ignored/file", b"old\n");
    let mut workspace = clone_workspace(base.path(), view.path(), "filter");

    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(base.path().join("advanced"), b"base moved\n").unwrap();
    fs::write(view.path().join("advanced"), b"workspace moved\n").unwrap();
    fs::write(view.path().join("Cargo.lock"), b"new\n").unwrap();
    fs::write(view.path().join(".git/index"), b"refreshed\n").unwrap();
    fs::write(view.path().join(".DS_Store"), b"touched\n").unwrap();
    fs::write(view.path().join("ignored/file"), b"ignored\n").unwrap();

    let default = collect_clone_changes(&workspace, false, true).unwrap();
    assert_eq!(default.len(), 2);
    assert!(default.iter().any(|change| {
        change.path == Path::new("Cargo.lock") && change.class == ChangeClass::Dependency
    }));
    let advanced = default
        .iter()
        .find(|change| change.path == Path::new("advanced"))
        .unwrap();
    assert_eq!(advanced.old_content, OldContent::BaseAdvanced);
    assert!(generate_unified_diff(&workspace, advanced).contains("base advanced"));

    let all = collect_clone_changes(&workspace, true, true).unwrap();
    assert_eq!(all.len(), 5);
    assert!(all
        .iter()
        .any(|change| change.class == ChangeClass::Internal));
    assert!(all
        .iter()
        .any(|change| change.class == ChangeClass::Runtime));

    workspace.manifest_status = ManifestStatus::Building;
    let pending = collect_clone_changes(&workspace, false, true).unwrap();
    assert!(pending
        .iter()
        .any(|change| change.path == Path::new("advanced")));
    assert!(!pending
        .iter()
        .any(|change| matches!(change.kind, ChangeKind::Deleted)));
}

#[test]
fn manifest_metadata_uses_nanosecond_precision() {
    let metadata = fs::metadata(env!("CARGO_MANIFEST_DIR")).unwrap();
    assert!(metadata.mtime() > 0);
    assert!((0..1_000_000_000).contains(&metadata.mtime_nsec()));
}

#[test]
fn stacking_semantics_are_a_backend_property() {
    assert!(!BackendKind::Overlay.stacking_is_snapshot());
    assert!(BackendKind::MacClone.stacking_is_snapshot());
    assert!(BackendKind::LinuxReflink.stacking_is_snapshot());
    assert!(BackendKind::PlainCopy.stacking_is_snapshot());
}

#[test]
#[serial]
fn copy_stack_is_a_snapshot_and_merges_exactly_one_level_at_a_time() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("file"), b"root\n").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(args)
            .output()
            .unwrap()
    };
    assert!(run(&[
        "fork",
        base.path().to_str().unwrap(),
        "--name",
        "parent",
        "--copy"
    ])
    .status
    .success());
    let parent_view = forkspace::state::State::load().unwrap().workspaces["parent"]
        .view_path
        .clone();
    fs::write(parent_view.join("file"), b"before-child\n").unwrap();
    assert!(run(&["fork", "parent", "--name", "child", "--copy"])
        .status
        .success());
    let child_view = forkspace::state::State::load().unwrap().workspaces["child"]
        .view_path
        .clone();
    fs::write(parent_view.join("file"), b"after-child\n").unwrap();
    assert_eq!(
        fs::read(child_view.join("file")).unwrap(),
        b"before-child\n"
    );
    fs::write(child_view.join("child-only"), b"child\n").unwrap();

    assert!(run(&["merge", "child", "--no-install", "--quiet"])
        .status
        .success());
    assert_eq!(
        fs::read(parent_view.join("child-only")).unwrap(),
        b"child\n"
    );
    assert!(!base.path().join("child-only").exists());
    assert!(run(&["merge", "parent", "--no-install", "--quiet"])
        .status
        .success());
    assert_eq!(
        fs::read(base.path().join("file")).unwrap(),
        b"after-child\n"
    );
    assert_eq!(
        fs::read(base.path().join("child-only")).unwrap(),
        b"child\n"
    );
}

#[test]
#[serial]
fn cross_platform_mutation_script_has_one_normalized_json_truth() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    for name in [
        "edit-a",
        "edit-b",
        "mode",
        "deleted",
        "renamed-old",
        "binary",
        "recreated",
        "cafe\u{301}",
        "Cargo.lock",
        ".git/index",
        ".DS_Store",
        "ignored/value",
        "tree/a",
        "tree/nested/b",
    ] {
        let path = base.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"old\n").unwrap();
    }
    std::os::unix::fs::symlink("edit-a", base.path().join("link")).unwrap();
    fs::write(base.path().join(".gitignore"), b"Cargo.lock\nignored/\n").unwrap();

    let fork = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args(["fork", base.path().to_str().unwrap(), "--name", "parity"])
        .output()
        .unwrap();
    assert!(
        fork.status.success(),
        "{}",
        String::from_utf8_lossy(&fork.stderr)
    );

    let workspace = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let workspace = forkspace::state::State::load()
                .unwrap()
                .workspaces
                .get("parity")
                .unwrap()
                .clone();
            if workspace.manifest_status != ManifestStatus::Building {
                break workspace;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    let view = &workspace.view_path;
    fs::write(view.join("edit-a"), b"new-a\n").unwrap();
    fs::write(view.join("edit-a"), b"newer-a\n").unwrap();
    fs::write(view.join("edit-b"), b"new-b\n").unwrap();
    let mut permissions = fs::metadata(view.join("mode")).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(view.join("mode"), permissions).unwrap();
    fs::remove_file(view.join("deleted")).unwrap();
    fs::remove_dir_all(view.join("tree")).unwrap();
    fs::rename(view.join("renamed-old"), view.join("renamed-new")).unwrap();
    fs::remove_file(view.join("link")).unwrap();
    std::os::unix::fs::symlink("edit-b", view.join("link")).unwrap();
    fs::write(view.join("binary"), [0, 1, 2, 3]).unwrap();
    fs::write(view.join("empty"), b"").unwrap();
    fs::write(view.join("cafe\u{301}"), b"nfd\n").unwrap();
    fs::write(view.join(".wh.literal"), b"ordinary\n").unwrap();
    fs::write(view.join("Cargo.lock"), b"lock\n").unwrap();
    fs::write(view.join(".git/index"), b"refreshed\n").unwrap();
    fs::write(view.join(".DS_Store"), b"finder\n").unwrap();
    fs::write(view.join("ignored/value"), b"ignored\n").unwrap();
    fs::remove_file(view.join("recreated")).unwrap();
    fs::write(view.join("recreated"), b"after\n").unwrap();
    fs::create_dir_all(view.join("created/nested")).unwrap();
    for index in 0..20 {
        fs::write(
            view.join(format!("created/nested/file-{index:02}")),
            b"new\n",
        )
        .unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args(["diff", "parity", "--json", "--no-gitignore"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut actual: Vec<(String, String, String)> = json["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| {
            (
                change["path"].as_str().unwrap().to_string(),
                change["kind"].as_str().unwrap().to_string(),
                change["class"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    actual.sort();

    let mut expected = vec![
        (".DS_Store", "modified", "runtime"),
        (".git/index", "modified", "internal"),
        (".wh.literal", "added", "source"),
        ("Cargo.lock", "modified", "dependency"),
        ("binary", "modified", "source"),
        ("cafe\u{301}", "modified", "source"),
        ("deleted", "deleted", "source"),
        ("edit-a", "modified", "source"),
        ("edit-b", "modified", "source"),
        ("empty", "added", "source"),
        ("ignored/value", "modified", "source"),
        ("link", "modified", "source"),
        ("mode", "modified", "source"),
        ("recreated", "modified", "source"),
        ("renamed-new", "added", "source"),
        ("renamed-old", "deleted", "source"),
        ("tree", "deleted", "source"),
    ]
    .into_iter()
    .map(|(path, kind, class)| (path.to_string(), kind.to_string(), class.to_string()))
    .collect::<Vec<_>>();
    expected.extend((0..20).map(|index| {
        (
            format!("created/nested/file-{index:02}"),
            "added".to_string(),
            "source".to_string(),
        )
    }));
    expected.sort();
    assert_eq!(actual, expected);

    for flag in ["--stat", "--files", "--full"] {
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(["diff", "parity", flag])
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag}");
        assert!(!output.stdout.is_empty(), "{flag}");
    }

    let drop = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args(["drop", "parity"])
        .output()
        .unwrap();
    assert!(drop.status.success());
}
