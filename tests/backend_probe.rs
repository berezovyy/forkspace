mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use forkspace::backend::{select_backend, BackendKind};
use forkspace::commands::fork::create_workspace_with_copy;
use forkspace::manifest;
use serial_test::serial;
use walkdir::WalkDir;

#[test]
#[serial]
fn plain_copy_is_a_privilege_free_fidelity_floor() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::create_dir_all(base.path().join("dir")).unwrap();
    fs::write(base.path().join("dir/file"), b"old\n").unwrap();
    fs::write(base.path().join("mode"), b"mode\n").unwrap();
    fs::write(base.path().join("edit"), b"old\n").unwrap();
    let mut permissions = fs::metadata(base.path().join("mode"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o750);
    fs::set_permissions(base.path().join("mode"), permissions).unwrap();
    std::os::unix::fs::symlink("dir/file", base.path().join("link")).unwrap();
    fs::hard_link(base.path().join("dir/file"), base.path().join("hardlink")).unwrap();

    let workspace = create_workspace_with_copy(
        base.path().to_path_buf(),
        "copy-floor",
        None,
        None,
        false,
        None,
        true,
    )
    .unwrap();
    assert_eq!(workspace.backend_kind, BackendKind::PlainCopy);
    assert!(workspace.degraded);
    assert!(workspace
        .probe_reason
        .as_deref()
        .unwrap()
        .contains("full copy"));
    assert_eq!(
        fs::read_link(workspace.view_path.join("link")).unwrap(),
        Path::new("dir/file")
    );
    assert_eq!(
        fs::metadata(workspace.view_path.join("mode"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    assert_eq!(
        fs::metadata(workspace.view_path.join("dir/file"))
            .unwrap()
            .ino(),
        fs::metadata(workspace.view_path.join("hardlink"))
            .unwrap()
            .ino()
    );
    let max_ctime = WalkDir::new(&workspace.view_path)
        .into_iter()
        .map(|entry| fs::symlink_metadata(entry.unwrap().path()).unwrap())
        .map(|metadata| {
            metadata.ctime().max(0) as u128 * 1_000_000_000 + metadata.ctime_nsec().max(0) as u128
        })
        .max()
        .unwrap();
    assert!(workspace.fork_time_ns >= max_ctime);
    let backend = select_backend(Some(workspace.backend_kind)).unwrap();
    assert!(backend.changes(&workspace, true, false).unwrap().is_empty());

    fs::write(workspace.view_path.join("edit"), b"changed\n").unwrap();
    let changes = backend.changes(&workspace, true, false).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, Path::new("edit"));
    let plan = forkspace::merge_plan::build(&workspace, false).unwrap();
    let journal = forkspace::merge_plan::execute(&plan).unwrap();
    forkspace::merge_plan::finish(&journal).unwrap();
    assert_eq!(fs::read(base.path().join("edit")).unwrap(), b"changed\n");

    forkspace::commands::drop::exec(forkspace::cli::DropArgs {
        name: Some("copy-floor".to_string()),
        force: false,
        all: false,
        glob: false,
    })
    .unwrap();
    assert!(!manifest::manifest_path("copy-floor").exists());
}

#[test]
#[serial]
fn reflink_probe_is_ioctl_based_and_override_is_sticky() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("file"), b"x").unwrap();
    let _honest_runtime_verdict = forkspace::backend::probe::reflink_supported(
        base.path(),
        &forkspace::util::forkspace_home().join("workspaces"),
    );
    let source = fs::read_to_string("src/backend/probe.rs").unwrap();
    assert!(source.contains("FICLONE"));
    assert!(!source.contains("f_type"));

    unsafe { std::env::set_var("FORKSPACE_BACKEND", "copy") };
    assert_eq!(select_backend(None).unwrap().kind(), BackendKind::PlainCopy);
    unsafe { std::env::remove_var("FORKSPACE_BACKEND") };
}
