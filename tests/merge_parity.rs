mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use forkspace::backend::{BackendKind, BackendState};
use forkspace::diff::collect_clone_changes;
use forkspace::error::ForkspaceError;
use forkspace::manifest::{self, ManifestStatus};
use forkspace::merge_plan;
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

fn plan(workspace: &Workspace, overwrite: bool) -> forkspace::error::Result<merge_plan::MergePlan> {
    merge_plan::build_from_changes(
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
    assert!(merge_plan::render(&plan).contains("conflicts: conflict"));
    assert_eq!(fs::read(base.path().join("unrelated")).unwrap(), b"old\n");
    let journal = merge_plan::execute(&plan).unwrap();
    merge_plan::finish(&journal).unwrap();
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
    let journal = merge_plan::execute(&plan).unwrap();
    let expected = tree_facts(base.path());

    fs::write(base.path().join("modified"), b"interrupted\n").unwrap();
    fs::write(base.path().join("deleted"), b"restored\n").unwrap();
    fs::remove_file(base.path().join("added")).unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    value["next"] = serde_json::json!(0);
    fs::write(&journal, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    merge_plan::resume(&workspace).unwrap();
    assert_eq!(tree_facts(base.path()), expected);
    merge_plan::finish(&journal).unwrap();
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
