mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use forkspace::backend::{BackendKind, BackendState};
use forkspace::changes::collect_clone_changes;
use forkspace::changes::manifest::{self, ManifestStatus};
use forkspace::error::ForkspaceError;
use forkspace::sync::plan;
use forkspace::util::now_unix_ns;
use forkspace::workspace::Workspace;
use serial_test::serial;

fn write_pair(base: &Path, view: &Path, path: &str, bytes: &[u8]) {
    fs::create_dir_all(base.join(path).parent().unwrap()).unwrap();
    fs::create_dir_all(view.join(path).parent().unwrap()).unwrap();
    fs::write(base.join(path), bytes).unwrap();
    fs::write(view.join(path), bytes).unwrap();
    let modified = fs::metadata(view.join(path)).unwrap().modified().unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(base.join(path))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
}

fn workspace(base: &Path, view: &Path, name: &str) -> Workspace {
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

fn plan(workspace: &Workspace, overwrite: bool) -> forkspace::error::Result<plan::MergePlan> {
    plan::build_from_changes(
        workspace,
        workspace.view_path.clone(),
        collect_clone_changes(workspace, true, false)?,
        overwrite,
    )
}

#[test]
#[serial]
fn conflicts_refuse_before_apply_and_overwrite_is_explicit() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "conflict", b"old\n");
    write_pair(base.path(), view.path(), "unrelated", b"old\n");
    let workspace = workspace(base.path(), view.path(), "conflict-plan");
    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(base.path().join("conflict"), b"base\n").unwrap();
    fs::write(view.path().join("conflict"), b"workspace\n").unwrap();
    fs::write(view.path().join("unrelated"), b"workspace\n").unwrap();

    let error = plan(&workspace, false).unwrap_err();
    assert!(matches!(error, ForkspaceError::MergeConflict { .. }));
    assert_eq!(fs::read(base.path().join("unrelated")).unwrap(), b"old\n");

    let plan = plan(&workspace, true).unwrap();
    assert_eq!(plan.conflicts, vec![std::path::PathBuf::from("conflict")]);
    assert!(plan::render(&plan).contains("conflicts: conflict"));
    assert_eq!(fs::read(base.path().join("unrelated")).unwrap(), b"old\n");
    let journal = plan::execute(&plan).unwrap();
    forkspace::sync::journal::remove(&journal).unwrap();
    assert_eq!(
        fs::read(base.path().join("conflict")).unwrap(),
        b"workspace\n"
    );
    assert_eq!(
        fs::read(base.path().join("unrelated")).unwrap(),
        b"workspace\n"
    );
}

#[test]
#[serial]
fn journal_resume_converges_for_files_deletes_modes_and_symlinks() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "modified", b"old\n");
    write_pair(base.path(), view.path(), "deleted", b"old\n");
    write_pair(base.path(), view.path(), "mode", b"same\n");
    std::os::unix::fs::symlink("modified", base.path().join("link")).unwrap();
    std::os::unix::fs::symlink("modified", view.path().join("link")).unwrap();
    let workspace = workspace(base.path(), view.path(), "resume-plan");
    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(view.path().join("modified"), b"new\n").unwrap();
    fs::write(view.path().join("added"), b"added\n").unwrap();
    fs::remove_file(view.path().join("deleted")).unwrap();
    let mut permissions = fs::metadata(view.path().join("mode"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(view.path().join("mode"), permissions).unwrap();
    fs::remove_file(view.path().join("link")).unwrap();
    std::os::unix::fs::symlink("added", view.path().join("link")).unwrap();

    let plan = plan(&workspace, false).unwrap();
    let journal = plan::execute(&plan).unwrap();
    let expected = tree_facts(base.path());

    fs::write(base.path().join("modified"), b"interrupted\n").unwrap();
    fs::write(base.path().join("deleted"), b"restored\n").unwrap();
    fs::remove_file(base.path().join("added")).unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    value["next"] = serde_json::json!(0);
    fs::write(&journal, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    plan::resume(&workspace).unwrap();
    assert_eq!(tree_facts(base.path()), expected);
    forkspace::sync::journal::remove(&journal).unwrap();
}

fn tree_facts(root: &Path) -> Vec<(String, Vec<u8>, u32)> {
    let mut facts = Vec::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.unwrap();
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .display()
            .to_string();
        let bytes = if entry.file_type().is_symlink() {
            fs::read_link(entry.path())
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else {
            fs::read(entry.path()).unwrap()
        };
        facts.push((
            relative,
            bytes,
            fs::symlink_metadata(entry.path())
                .unwrap()
                .permissions()
                .mode(),
        ));
    }
    facts.sort_by(|left, right| left.0.cmp(&right.0));
    facts
}

#[test]
#[serial]
fn building_refuses_and_dirty_rows_are_ancestor_unknown_not_conflicts() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "raced", b"old\n");
    let mut workspace = workspace(base.path(), view.path(), "dirty-plan");
    workspace.manifest_status = ManifestStatus::Building;
    assert!(plan(&workspace, false).is_err());

    workspace.manifest_status = ManifestStatus::Ready;
    let path = manifest::manifest_path("dirty-plan");
    let mut rows = manifest::read("dirty-plan").unwrap();
    rows.iter_mut()
        .find(|row| row.path == "raced")
        .unwrap()
        .dirty = true;
    fs::write(
        &path,
        rows.into_iter()
            .map(|row| serde_json::to_string(&row).unwrap() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    fs::write(base.path().join("raced"), b"base moved\n").unwrap();
    fs::write(view.path().join("raced"), b"workspace moved\n").unwrap();
    let plan = plan(&workspace, false).unwrap();
    assert!(plan.conflicts.is_empty());
    assert_eq!(
        plan.ancestor_unknown,
        vec![std::path::PathBuf::from("raced")]
    );
}

#[test]
#[serial]
fn selected_directory_backend_round_trips_through_the_public_cli() {
    let home = common::setup_test_env();
    let inherited_backend = std::env::var_os("FORKSPACE_BACKEND");
    if inherited_backend.is_none() {
        unsafe { std::env::set_var("FORKSPACE_BACKEND", "copy") };
    }
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("edit"), b"old\n").unwrap();
    fs::write(base.path().join("delete"), b"old\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args([
            "fork",
            base.path().to_str().unwrap(),
            "--name",
            "primitive-parity",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let workspace = forkspace::state::State::load()
        .unwrap()
        .workspaces
        .get("primitive-parity")
        .unwrap()
        .clone();
    assert!(matches!(
        workspace.backend_kind,
        BackendKind::MacClone | BackendKind::LinuxReflink | BackendKind::PlainCopy
    ));
    fs::write(workspace.view_path.join("edit"), b"new\n").unwrap();
    fs::write(workspace.view_path.join("add"), b"new\n").unwrap();
    fs::remove_file(workspace.view_path.join("delete")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args(["merge", "primitive-parity", "--no-install"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(base.path().join("edit")).unwrap(), b"new\n");
    assert_eq!(fs::read(base.path().join("add")).unwrap(), b"new\n");
    assert!(!base.path().join("delete").exists());
    if inherited_backend.is_none() {
        unsafe { std::env::remove_var("FORKSPACE_BACKEND") };
    }
}

#[test]
#[serial]
fn installer_failure_keeps_the_applied_journal_resumable_and_never_drops() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("source"), b"old\n").unwrap();
    fs::write(
        base.path().join("package.json"),
        br#"{"name":"merge-fixture","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        base.path().join("package-lock.json"),
        br#"{"lockfileVersion":3,"packages":{"":{}}}"#,
    )
    .unwrap();
    let forked = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args([
            "fork",
            base.path().to_str().unwrap(),
            "--name",
            "install-failure",
            "--copy",
        ])
        .output()
        .unwrap();
    assert!(forked.status.success());
    let view = forkspace::state::State::load()
        .unwrap()
        .require_workspace("install-failure")
        .unwrap()
        .view_path
        .clone();
    fs::write(view.join("source"), b"new\n").unwrap();
    fs::write(
        view.join("package-lock.json"),
        br#"{"lockfileVersion":3,"packages":{"":{},"node_modules/zod":{}}}"#,
    )
    .unwrap();

    let tools = tempfile::tempdir().unwrap();
    let npm = tools.path().join("npm");
    fs::write(&npm, "#!/bin/sh\nexit 23\n").unwrap();
    fs::set_permissions(&npm, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        tools.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let failed = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .env("PATH", &path)
        .args(["merge", "install-failure", "--drop"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("package manager failed"));
    assert_eq!(fs::read(base.path().join("source")).unwrap(), b"new\n");
    assert!(forkspace::state::State::load()
        .unwrap()
        .workspaces
        .contains_key("install-failure"));
    assert!(fs::read_dir(home.path().join("journal"))
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("merge-install-failure-")));

    fs::write(&npm, "#!/bin/sh\nexit 0\n").unwrap();
    let resumed = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .env("PATH", path)
        .args(["merge", "install-failure", "--resume", "--drop"])
        .output()
        .unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(!forkspace::state::State::load()
        .unwrap()
        .workspaces
        .contains_key("install-failure"));
    assert!(fs::read_dir(home.path().join("journal"))
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true));
}
