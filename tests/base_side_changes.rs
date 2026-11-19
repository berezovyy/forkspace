//! Base-side changes a direct `merge` must not overwrite: edits to files the workspace wrote before
//! the manifest walk reached them (dirty rows), and base rewrites that keep size and mtime.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use forkspace::backend::{BackendKind, BackendState};
use forkspace::changes::collect_clone_changes;
use forkspace::changes::manifest::{self, ManifestStatus};
use forkspace::error::ForkspaceError;
use forkspace::sync::{ancestor, journal, merge, plan};
use forkspace::workspace::Workspace;
use serial_test::serial;

fn fork(base: &Path, view: &Path, name: &str) -> Workspace {
    let status = Command::new("cp")
        .arg("-Rp")
        .arg(format!("{}/.", base.display()))
        .arg(view)
        .status()
        .unwrap();
    assert!(status.success());
    let mut workspace = Workspace::new(name, base.to_path_buf(), None);
    workspace.backend_kind = BackendKind::MacClone;
    workspace.backend_state = BackendState::MacClone {
        unknown_fields: serde_json::Map::new(),
    };
    workspace.view_path = view.to_path_buf();
    std::thread::sleep(std::time::Duration::from_millis(5));
    workspace.fork_time_ns = forkspace::util::fs::ctime_clock_ns();
    workspace.manifest_status = ManifestStatus::Ready;
    ancestor::record(name, view).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    workspace
}

fn build_manifest(workspace: &Workspace) {
    manifest::build(
        &workspace.name,
        &workspace.view_path,
        workspace.fork_time_ns,
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
}

fn plan(workspace: &Workspace) -> forkspace::error::Result<plan::MergePlan> {
    plan::build_from_changes(
        workspace,
        workspace.view_path.clone(),
        collect_clone_changes(workspace, true, false)?,
        false,
    )
}

fn apply(plan: &plan::MergePlan) {
    let journal = merge::execute(plan).unwrap();
    journal::remove(&journal).unwrap();
}

fn lines(edit: Option<(usize, &str)>) -> String {
    (1..=10)
        .map(|n| match edit {
            Some((line, text)) if line == n => format!("{text}\n"),
            _ => format!("line{n}\n"),
        })
        .collect()
}

fn is_conflict(error: &ForkspaceError) -> bool {
    matches!(
        error,
        ForkspaceError::MergeConflict { .. } | ForkspaceError::MergeOverlap { .. }
    )
}

/// DATA-12: before the fix the workspace's early version replaced the base's edit with exit 0.
#[test]
#[serial]
fn a_base_edit_to_a_file_written_before_the_walk_is_a_conflict() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::write(base.path().join("x"), "orig\n").unwrap();
    let workspace = fork(base.path(), view.path(), "early-same-lines");
    fs::write(view.path().join("x"), "workspace\n").unwrap();
    build_manifest(&workspace);
    fs::write(base.path().join("x"), "base\n").unwrap();

    let error = plan(&workspace).unwrap_err();
    assert!(is_conflict(&error), "{error:?}");
    assert_eq!(fs::read_to_string(base.path().join("x")).unwrap(), "base\n");
}

/// DATA-12, lines apart: both edits survive, merged against the fork-time copy.
#[test]
#[serial]
fn edits_on_different_lines_of_an_early_written_file_merge_line_by_line() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::write(base.path().join("x"), lines(None)).unwrap();
    let workspace = fork(base.path(), view.path(), "early-lines");
    fs::write(view.path().join("x"), lines(Some((10, "WS10")))).unwrap();
    build_manifest(&workspace);
    fs::write(base.path().join("x"), lines(Some((1, "BASE1")))).unwrap();

    let plan = plan(&workspace).unwrap();
    assert_eq!(plan.auto_merged, vec![std::path::PathBuf::from("x")]);
    apply(&plan);
    let merged = fs::read_to_string(base.path().join("x")).unwrap();
    assert!(
        merged.starts_with("BASE1\n") && merged.ends_with("WS10\n"),
        "{merged}"
    );
}

/// Not over-strict: an early-written file over an untouched base merges without a conflict.
#[test]
#[serial]
fn an_early_written_file_over_an_untouched_base_merges() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::write(base.path().join("x"), "orig\n").unwrap();
    let workspace = fork(base.path(), view.path(), "early-untouched");
    fs::write(view.path().join("x"), "workspace\n").unwrap();
    build_manifest(&workspace);

    let plan = plan(&workspace).unwrap();
    assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
    apply(&plan);
    assert_eq!(
        fs::read_to_string(base.path().join("x")).unwrap(),
        "workspace\n"
    );
}

/// DATA-13: a base rewrite with the same size and the old mtime was invisible and overwritten.
#[test]
#[serial]
fn a_base_rewrite_that_keeps_size_and_mtime_is_a_conflict() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::write(base.path().join("x"), "AAAA\n").unwrap();
    let workspace = fork(base.path(), view.path(), "same-size");
    build_manifest(&workspace);
    fs::write(view.path().join("x"), "WWWW\n").unwrap();
    let mtime = fs::metadata(base.path().join("x"))
        .unwrap()
        .modified()
        .unwrap();
    fs::write(base.path().join("x"), "BBBB\n").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(base.path().join("x"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    assert_eq!(
        fs::metadata(base.path().join("x"))
            .unwrap()
            .modified()
            .unwrap(),
        mtime
    );

    let error = plan(&workspace).unwrap_err();
    assert!(is_conflict(&error), "{error:?}");
    assert_eq!(fs::read_to_string(base.path().join("x")).unwrap(), "BBBB\n");
}

/// MERGE-13: one early-written file inside a directory the workspace then deleted made the
/// deletion a conflict although the base was untouched.
#[test]
#[serial]
fn deleting_a_directory_with_an_early_written_file_is_no_conflict() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::create_dir(base.path().join("dd")).unwrap();
    fs::write(base.path().join("dd/a"), "a\n").unwrap();
    fs::write(base.path().join("dd/b"), "b\n").unwrap();
    let workspace = fork(base.path(), view.path(), "early-then-delete");
    fs::write(view.path().join("dd/a"), "a2\n").unwrap();
    build_manifest(&workspace);
    fs::remove_dir_all(view.path().join("dd")).unwrap();

    let plan = plan(&workspace).unwrap();
    assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
    apply(&plan);
    assert!(!base.path().join("dd").exists());
}

/// A base file deleted since the fork, which the workspace wrote early, is a conflict, not a
/// re-addition.
#[test]
#[serial]
fn a_base_deletion_of_an_early_written_file_is_a_conflict() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::write(base.path().join("x"), "orig\n").unwrap();
    fs::write(base.path().join("k"), "k\n").unwrap();
    let workspace = fork(base.path(), view.path(), "early-base-deleted");
    fs::write(view.path().join("x"), "workspace\n").unwrap();
    build_manifest(&workspace);
    fs::remove_file(base.path().join("x")).unwrap();

    let error = plan(&workspace).unwrap_err();
    assert!(is_conflict(&error), "{error:?}");
    assert!(!base.path().join("x").exists());
}

/// MERGE-14: a line merge staged the result with the workspace's mode, dropping a base chmod.
#[test]
#[serial]
fn a_base_mode_change_survives_a_line_merge() {
    use std::os::unix::fs::PermissionsExt;
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    fs::write(base.path().join("x"), lines(None)).unwrap();
    fs::set_permissions(base.path().join("x"), fs::Permissions::from_mode(0o644)).unwrap();
    let workspace = fork(base.path(), view.path(), "base-chmod");
    build_manifest(&workspace);
    fs::write(view.path().join("x"), lines(Some((10, "WS10")))).unwrap();
    fs::set_permissions(base.path().join("x"), fs::Permissions::from_mode(0o755)).unwrap();

    let plan = plan(&workspace).unwrap();
    apply(&plan);
    let merged = base.path().join("x");
    assert!(fs::read_to_string(&merged).unwrap().ends_with("WS10\n"));
    assert_eq!(
        fs::metadata(&merged).unwrap().permissions().mode() & 0o777,
        0o755
    );
}
