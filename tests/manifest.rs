mod common;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use forkspace::backend::ChangeCount;
use forkspace::manifest::{self, ManifestKind, ManifestStatus};
use forkspace::state::State;
use forkspace::util::now_unix_ns;
use serial_test::serial;

#[test]
#[serial]
fn manifest_build_is_atomic_and_rows_match_tree() {
    let _home = common::setup_test_env();
    let view = tempfile::tempdir().unwrap();
    fs::create_dir_all(view.path().join("src")).unwrap();
    fs::write(view.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    std::os::unix::fs::symlink("src/main.rs", view.path().join("main-link")).unwrap();

    let count = manifest::build("rows", view.path(), u128::MAX).unwrap();
    let rows = manifest::read("rows").unwrap();

    assert_eq!(count, 3);
    assert_eq!(rows.len(), 3);
    let file = rows.iter().find(|row| row.path == "src/main.rs").unwrap();
    assert_eq!(file.kind, ManifestKind::File);
    assert_eq!(file.size, 13);
    assert!(file.mtime_ns > 0);
    assert!(!file.dirty);
    let link = rows.iter().find(|row| row.path == "main-link").unwrap();
    assert_eq!(link.kind, ManifestKind::Symlink);
    assert_eq!(link.target.as_deref(), Some("src/main.rs"));
    assert!(
        !fs::read_dir(manifest::manifest_path("rows").parent().unwrap())
            .unwrap()
            .any(|entry| entry.unwrap().file_name().to_string_lossy().contains("tmp"))
    );
}

#[test]
#[serial]
fn manifest_round_trips_non_utf8_long_and_nfd_paths() {
    let _home = common::setup_test_env();
    let view = tempfile::tempdir().unwrap();
    let non_utf = OsString::from_vec(vec![b'n', b'o', b'n', b'-', 0xff]);
    fs::write(view.path().join(&non_utf), b"bytes").unwrap();
    let nfd = "cafe\u{301}";
    fs::write(view.path().join(nfd), b"nfd").unwrap();
    let mut long = PathBuf::new();
    for index in 0..6 {
        long.push(format!("{index}-{}", "x".repeat(150)));
    }
    fs::create_dir_all(view.path().join(&long)).unwrap();
    long.push("leaf");
    fs::write(view.path().join(&long), b"long").unwrap();

    manifest::build("paths", view.path(), u128::MAX).unwrap();
    let decoded: Vec<_> = manifest::read("paths")
        .unwrap()
        .into_iter()
        .map(|row| manifest::decode_path(&row.path).unwrap())
        .collect();

    assert!(decoded
        .iter()
        .any(|path| path.as_os_str().as_bytes() == non_utf.as_os_str().as_bytes()));
    assert!(decoded.iter().any(|path| path == Path::new(nfd)));
    assert!(decoded.iter().any(|path| path == &long));
}

#[test]
#[serial]
fn dirty_guard_and_line_level_corruption_are_detected() {
    let _home = common::setup_test_env();
    let view = tempfile::tempdir().unwrap();
    let fork_time = now_unix_ns();
    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(view.path().join("raced"), b"after fork").unwrap();
    manifest::build("dirty", view.path(), fork_time).unwrap();
    assert!(manifest::read("dirty").unwrap()[0].dirty);

    fs::OpenOptions::new()
        .append(true)
        .open(manifest::manifest_path("dirty"))
        .unwrap()
        .write_all(b"{truncated\n")
        .unwrap();
    let message = manifest::read("dirty").unwrap_err().to_string();
    assert!(message.contains("line 2"));
}

#[test]
#[serial]
fn invalid_manifest_after_writes_marks_tracking_degraded() {
    let home = common::setup_test_env();
    let view = tempfile::tempdir().unwrap();
    fs::write(view.path().join("file"), b"base").unwrap();
    let mut workspace =
        forkspace::workspace::Workspace::new("degraded", view.path().to_path_buf(), None);
    workspace.view_path = view.path().to_path_buf();
    workspace.fork_time_ns = now_unix_ns();
    workspace.manifest_status = ManifestStatus::Ready;
    manifest::build("degraded", view.path(), u128::MAX).unwrap();
    let mut state = State::default();
    state.add_workspace(workspace.clone()).unwrap();
    state.save().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(view.path().join("file"), b"changed").unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(manifest::manifest_path("degraded"))
        .unwrap()
        .write_all(b"broken\n")
        .unwrap();

    assert_eq!(
        manifest::change_count(&workspace, false).unwrap(),
        ChangeCount::Degraded
    );
    assert_eq!(
        State::load()
            .unwrap()
            .workspaces
            .get("degraded")
            .unwrap()
            .manifest_status,
        ManifestStatus::Degraded
    );
    assert!(home.path().join("manifests/degraded.jsonl").exists());
}

#[test]
#[serial]
fn invalid_manifest_after_deletion_marks_tracking_degraded() {
    let _home = common::setup_test_env();
    let view = tempfile::tempdir().unwrap();
    fs::write(view.path().join("deleted"), b"base").unwrap();
    let mut workspace =
        forkspace::workspace::Workspace::new("deleted", view.path().to_path_buf(), None);
    workspace.view_path = view.path().to_path_buf();
    std::thread::sleep(std::time::Duration::from_millis(2));
    workspace.fork_time_ns = now_unix_ns();
    workspace.manifest_status = ManifestStatus::Ready;
    manifest::build("deleted", view.path(), workspace.fork_time_ns).unwrap();
    let mut state = State::default();
    state.add_workspace(workspace.clone()).unwrap();
    state.save().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::remove_file(view.path().join("deleted")).unwrap();
    fs::write(manifest::manifest_path("deleted"), b"truncated").unwrap();

    assert_eq!(
        manifest::change_count(&workspace, false).unwrap(),
        ChangeCount::Degraded
    );
}

#[test]
#[serial]
fn ready_manifest_counts_changes_and_respects_ignore_files() {
    let _home = common::setup_test_env();
    let view = tempfile::tempdir().unwrap();
    fs::write(view.path().join("one"), b"base").unwrap();
    fs::write(view.path().join("two"), b"base").unwrap();
    fs::write(view.path().join("ignored"), b"base").unwrap();
    fs::write(view.path().join(".gitignore"), b"ignored\n").unwrap();
    manifest::build("count", view.path(), u128::MAX).unwrap();

    let mut workspace =
        forkspace::workspace::Workspace::new("count", view.path().to_path_buf(), None);
    workspace.view_path = view.path().to_path_buf();
    workspace.fork_time_ns = now_unix_ns();
    workspace.manifest_status = ManifestStatus::Ready;
    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(view.path().join("one"), b"changed").unwrap();
    fs::remove_file(view.path().join("two")).unwrap();
    fs::write(view.path().join("three"), b"new").unwrap();
    fs::write(view.path().join("ignored"), b"changed").unwrap();

    assert_eq!(
        manifest::change_count(&workspace, false).unwrap(),
        ChangeCount::Ready(3)
    );
    assert_eq!(
        manifest::change_count(&workspace, true).unwrap(),
        ChangeCount::Ready(4)
    );
}

#[test]
#[serial]
fn corrupt_manifest_before_writes_returns_to_building() {
    let _home = common::setup_test_env();
    let view = tempfile::tempdir().unwrap();
    fs::write(view.path().join("file"), b"base").unwrap();
    manifest::build("rebuild", view.path(), u128::MAX).unwrap();
    let mut workspace =
        forkspace::workspace::Workspace::new("rebuild", view.path().to_path_buf(), None);
    workspace.view_path = view.path().to_path_buf();
    std::thread::sleep(std::time::Duration::from_millis(2));
    workspace.fork_time_ns = now_unix_ns();
    workspace.manifest_status = ManifestStatus::Ready;
    let mut state = State::default();
    state.add_workspace(workspace.clone()).unwrap();
    state.save().unwrap();
    fs::write(manifest::manifest_path("rebuild"), b"truncated").unwrap();

    assert_eq!(
        manifest::change_count(&workspace, false).unwrap(),
        ChangeCount::Pending
    );
    assert_eq!(
        State::load()
            .unwrap()
            .workspaces
            .get("rebuild")
            .unwrap()
            .manifest_status,
        ManifestStatus::Building
    );
}

#[test]
fn malformed_non_ascii_escape_is_rejected_without_panicking() {
    assert!(manifest::decode_path("%aé").is_err());
}
