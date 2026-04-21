mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use forkspace::backend::{BackendKind, BackendState};
use forkspace::diff::collect_clone_changes;
use forkspace::error::ForkspaceError;
use forkspace::manifest::{self, ManifestStatus};
use forkspace::pull;
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
    let mut ws = Workspace::new(name, base.to_path_buf(), None);
    ws.backend_kind = BackendKind::MacClone;
    ws.backend_state = BackendState::MacClone {
        unknown_fields: serde_json::Map::new(),
    };
    ws.view_path = view.to_path_buf();
    std::thread::sleep(std::time::Duration::from_millis(2));
    ws.fork_time_ns = now_unix_ns();
    ws.manifest_status = ManifestStatus::Ready;
    ws.manifest_generation = 1;
    manifest::build(name, view, ws.fork_time_ns).unwrap();
    ws
}

fn build_plan(ws: &Workspace, theirs: bool) -> forkspace::error::Result<pull::PullPlan> {
    pull::build_from_changes(
        ws,
        pull::base_changes(ws)?,
        collect_clone_changes(ws, true, false)?,
        theirs,
    )
}

#[test]
#[serial]
fn clean_fast_forward_rebaselines_files_deletes_modes_and_symlinks() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    for name in ["edit", "delete", "mode"] {
        write_pair(base.path(), view.path(), name, b"old\n");
    }
    std::os::unix::fs::symlink("edit", base.path().join("link")).unwrap();
    std::os::unix::fs::symlink("edit", view.path().join("link")).unwrap();
    let mut ws = workspace(base.path(), view.path(), "pull-clean");
    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(base.path().join("edit"), b"new\n").unwrap();
    fs::write(base.path().join("add"), b"added\n").unwrap();
    fs::remove_file(base.path().join("delete")).unwrap();
    let mut permissions = fs::metadata(base.path().join("mode"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(base.path().join("mode"), permissions).unwrap();
    fs::remove_file(base.path().join("link")).unwrap();
    std::os::unix::fs::symlink("add", base.path().join("link")).unwrap();

    let plan = build_plan(&ws, false).unwrap();
    assert!(pull::render(&plan).contains("pull plan"));
    let journal = pull::start(&ws, &plan).unwrap();
    pull::finish(&mut ws, &journal).unwrap();
    pull::complete(&journal).unwrap();

    assert_eq!(fs::read(view.path().join("edit")).unwrap(), b"new\n");
    assert_eq!(fs::read(view.path().join("add")).unwrap(), b"added\n");
    assert!(!view.path().join("delete").exists());
    assert_eq!(
        fs::read_link(view.path().join("link")).unwrap(),
        Path::new("add")
    );
    assert_eq!(
        fs::metadata(view.path().join("mode"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert!(collect_clone_changes(&ws, true, false).unwrap().is_empty());
    assert_eq!(ws.manifest_generation, 2);
    assert!(ws.last_pull.is_some());
}

#[test]
#[serial]
fn disjoint_edits_preserve_ours_and_conflicts_refuse_atomically() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "a", b"old-a\n");
    write_pair(base.path(), view.path(), "b", b"old-b\n");
    let mut ws = workspace(base.path(), view.path(), "pull-disjoint");
    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(base.path().join("a"), b"base-a\n").unwrap();
    fs::write(view.path().join("b"), b"ours-b\n").unwrap();

    let plan = build_plan(&ws, false).unwrap();
    assert_eq!(plan.changes.len(), 1);
    let journal = pull::start(&ws, &plan).unwrap();
    pull::finish(&mut ws, &journal).unwrap();
    pull::complete(&journal).unwrap();
    assert_eq!(fs::read(view.path().join("a")).unwrap(), b"base-a\n");
    assert_eq!(fs::read(view.path().join("b")).unwrap(), b"ours-b\n");
    let remaining = collect_clone_changes(&ws, true, false).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].path, Path::new("b"));

    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(base.path().join("a"), b"base-again\n").unwrap();
    fs::write(view.path().join("a"), b"ours-a\n").unwrap();
    let before = fs::read(view.path().join("a")).unwrap();
    let error = build_plan(&ws, false).unwrap_err();
    assert!(matches!(error, ForkspaceError::PullConflict { .. }));
    assert_eq!(fs::read(view.path().join("a")).unwrap(), before);

    let plan = build_plan(&ws, true).unwrap();
    let journal = pull::start(&ws, &plan).unwrap();
    pull::finish(&mut ws, &journal).unwrap();
    pull::complete(&journal).unwrap();
    assert_eq!(fs::read(view.path().join("a")).unwrap(), b"base-again\n");
}

#[test]
#[serial]
fn open_journal_blocks_stale_ancestor_operations_and_resume_converges() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "a", b"old\n");
    let mut ws = workspace(base.path(), view.path(), "pull-resume");
    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(base.path().join("a"), b"new\n").unwrap();
    let plan = build_plan(&ws, false).unwrap();
    let journal = pull::start(&ws, &plan).unwrap();
    assert!(matches!(
        pull::guard(&ws.name),
        Err(ForkspaceError::StaleAncestor(_))
    ));
    let resumed = pull::resume_apply(&ws).unwrap();
    assert_eq!(resumed, journal);
    pull::finish(&mut ws, &journal).unwrap();
    pull::complete(&journal).unwrap();
    pull::guard(&ws.name).unwrap();
    assert_eq!(fs::read(view.path().join("a")).unwrap(), b"new\n");
}
