mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use forkspace::backend::{BackendKind, BackendState};
use forkspace::changes::collect_clone_changes;
use forkspace::changes::manifest::{self, ManifestStatus};
use forkspace::error::ForkspaceError;
use forkspace::sync::pull;
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

/// Wait out one tick of the clock Linux stamps ctimes from (1–10 ms behind the fine clock), so
/// a write before this call is never mistaken for one made after it.
fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(15));
}

fn workspace(base: &Path, view: &Path, name: &str) -> Workspace {
    let mut ws = Workspace::new(name, base.to_path_buf(), None);
    // A snapshot backend this build has: pull dispatches to it when the base changed while
    // the pull read it.
    if cfg!(target_os = "macos") {
        ws.backend_kind = BackendKind::MacClone;
        ws.backend_state = BackendState::MacClone {
            unknown_fields: serde_json::Map::new(),
        };
    } else {
        ws.backend_kind = BackendKind::PlainCopy;
        ws.backend_state = BackendState::CowDir {
            reflinked_files: 0,
            copied_files: 0,
            unknown_fields: serde_json::Map::new(),
        };
    }
    ws.view_path = view.to_path_buf();
    tick();
    ws.fork_time_ns = forkspace::util::fs::ctime_clock_ns();
    ws.manifest_status = ManifestStatus::Ready;
    ws.manifest_generation = 1;
    manifest::build(name, view, ws.fork_time_ns).unwrap();
    ws
}

fn build_plan(ws: &Workspace) -> forkspace::error::Result<pull::PullPlan> {
    tick();
    pull::build_from_changes(
        ws,
        pull::base_changes(ws)?,
        collect_clone_changes(ws, true, false)?,
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
    tick();
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

    let plan = build_plan(&ws).unwrap();
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
    tick();
    fs::write(base.path().join("a"), b"base-a\n").unwrap();
    fs::write(view.path().join("b"), b"ours-b\n").unwrap();

    let plan = build_plan(&ws).unwrap();
    assert_eq!(plan.changes.len(), 1);
    let journal = pull::start(&ws, &plan).unwrap();
    pull::finish(&mut ws, &journal).unwrap();
    pull::complete(&journal).unwrap();
    assert_eq!(fs::read(view.path().join("a")).unwrap(), b"base-a\n");
    assert_eq!(fs::read(view.path().join("b")).unwrap(), b"ours-b\n");
    let remaining = collect_clone_changes(&ws, true, false).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].path, Path::new("b"));

    // Binary content cannot be merged line by line, so it stays a whole-file conflict.
    tick();
    fs::write(base.path().join("a"), b"base-again\0\n").unwrap();
    fs::write(view.path().join("a"), b"ours-a\0\n").unwrap();
    let before = fs::read(view.path().join("a")).unwrap();
    let plan = build_plan(&ws).unwrap();
    // Planning changes nothing; the plan takes the base's version of what cannot merge.
    assert_eq!(plan.conflicts, [Path::new("a")]);
    assert_eq!(fs::read(view.path().join("a")).unwrap(), before);
    let journal = pull::start(&ws, &plan).unwrap();
    pull::finish(&mut ws, &journal).unwrap();
    pull::complete(&journal).unwrap();
    assert_eq!(fs::read(view.path().join("a")).unwrap(), b"base-again\0\n");
}

#[test]
#[serial]
fn open_journal_blocks_stale_ancestor_operations_and_resume_converges() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "a", b"old\n");
    let mut ws = workspace(base.path(), view.path(), "pull-resume");
    tick();
    fs::write(base.path().join("a"), b"new\n").unwrap();
    let plan = build_plan(&ws).unwrap();
    let journal = pull::start(&ws, &plan).unwrap();
    assert!(matches!(
        forkspace::workspace::guard::ensure_no_interrupted_pull(&ws.name),
        Err(ForkspaceError::StaleAncestor(_))
    ));
    let resumed = pull::resume_apply(&ws).unwrap();
    assert_eq!(resumed, journal);
    pull::finish(&mut ws, &journal).unwrap();
    pull::complete(&journal).unwrap();
    forkspace::workspace::guard::ensure_no_interrupted_pull(&ws.name).unwrap();
    assert_eq!(fs::read(view.path().join("a")).unwrap(), b"new\n");
}

#[test]
#[serial]
fn conflict_kind_matrix_refuses_without_partial_application() {
    let _home = common::setup_test_env();

    let run = |name: &str, prepare: fn(&Path, &Path), mutate: fn(&Path, &Path)| {
        let base = tempfile::tempdir().unwrap();
        let view = tempfile::tempdir().unwrap();
        prepare(base.path(), view.path());
        let ws = workspace(base.path(), view.path(), name);
        tick();
        mutate(base.path(), view.path());
        let before = tree_facts(view.path());
        assert!(!build_plan(&ws).unwrap().conflicts.is_empty());
        assert_eq!(tree_facts(view.path()), before);
    };

    run(
        "base-delete-ours-modify",
        |base, view| write_pair(base, view, "path", b"old\n"),
        |base, view| {
            fs::remove_file(base.join("path")).unwrap();
            fs::write(view.join("path"), b"ours\n").unwrap();
        },
    );
    run(
        "base-modify-ours-delete",
        |base, view| write_pair(base, view, "path", b"old\n"),
        |base, view| {
            fs::write(base.join("path"), b"theirs\n").unwrap();
            fs::remove_file(view.join("path")).unwrap();
        },
    );
    run(
        "file-directory-replacement",
        |base, view| write_pair(base, view, "path", b"old\n"),
        |base, view| {
            fs::remove_file(base.join("path")).unwrap();
            fs::create_dir(base.join("path")).unwrap();
            fs::write(base.join("path/child"), b"theirs\n").unwrap();
            fs::write(view.join("path"), b"ours\n").unwrap();
        },
    );
    run(
        "symlink-retarget",
        |base, view| {
            fs::write(base.join("a"), b"a").unwrap();
            fs::write(view.join("a"), b"a").unwrap();
            std::os::unix::fs::symlink("a", base.join("path")).unwrap();
            std::os::unix::fs::symlink("a", view.join("path")).unwrap();
        },
        |base, view| {
            fs::remove_file(base.join("path")).unwrap();
            fs::remove_file(view.join("path")).unwrap();
            std::os::unix::fs::symlink("theirs", base.join("path")).unwrap();
            std::os::unix::fs::symlink("ours", view.join("path")).unwrap();
        },
    );
}

#[test]
#[serial]
fn base_deleted_directory_keeps_files_the_workspace_added_inside() {
    // DATA-01: deleting the view's directory recursively lost the workspace's additions.
    let _home = common::setup_test_env();
    for theirs in [false, true] {
        let base = tempfile::tempdir().unwrap();
        let view = tempfile::tempdir().unwrap();
        write_pair(base.path(), view.path(), "dir/keep.txt", b"keep\n");
        write_pair(base.path(), view.path(), "dir/sub/k2.txt", b"k2\n");
        write_pair(base.path(), view.path(), "gone/old.txt", b"old\n");
        let name = format!("pull-kept-dir-{theirs}");
        let mut ws = workspace(base.path(), view.path(), &name);
        tick();
        fs::write(view.path().join("dir/new.txt"), b"ours\n").unwrap();
        fs::write(view.path().join("dir/sub/new2.txt"), b"ours too\n").unwrap();
        if theirs {
            // A conflict taken toward the base still must not take the additions with it.
            fs::write(view.path().join("dir/keep.txt"), b"ours edit\n").unwrap();
        }
        fs::remove_dir_all(base.path().join("dir")).unwrap();
        fs::remove_dir_all(base.path().join("gone")).unwrap();

        let plan = build_plan(&ws).unwrap();
        let planned: Vec<_> = plan.changes.iter().map(|c| c.path.clone()).collect();
        assert!(!planned.contains(&"dir".into()), "{planned:?}");
        assert!(!planned.contains(&"dir/sub".into()), "{planned:?}");
        assert!(planned.contains(&"gone".into()), "{planned:?}");
        assert_eq!(plan.kept, [Path::new("dir"), Path::new("dir/sub")]);
        assert!(pull::render(&plan).contains("kept for workspace changes: dir, dir/sub"));
        let journal = pull::start(&ws, &plan).unwrap();
        pull::finish(&mut ws, &journal).unwrap();
        pull::complete(&journal).unwrap();

        assert_eq!(
            fs::read(view.path().join("dir/new.txt")).unwrap(),
            b"ours\n"
        );
        assert_eq!(
            fs::read(view.path().join("dir/sub/new2.txt")).unwrap(),
            b"ours too\n"
        );
        assert!(!view.path().join("dir/keep.txt").exists());
        assert!(!view.path().join("dir/sub/k2.txt").exists());
        assert!(!view.path().join("gone").exists());
        let mut remaining: Vec<_> = collect_clone_changes(&ws, true, false)
            .unwrap()
            .into_iter()
            .filter(|change| change.kind == forkspace::changes::ChangeKind::Added)
            .map(|change| change.path)
            .collect();
        remaining.sort();
        assert!(remaining.contains(&"dir/new.txt".into()), "{remaining:?}");
        assert!(
            remaining.contains(&"dir/sub/new2.txt".into()),
            "{remaining:?}"
        );
    }
}

#[test]
#[serial]
fn base_replacing_a_directory_conflicts_with_workspace_additions_inside() {
    // DATA-01's sibling: a directory the base turned into a file was swapped out whole.
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "dir/keep.txt", b"keep\n");
    write_pair(base.path(), view.path(), "dir/sub/k2.txt", b"k2\n");
    let ws = workspace(base.path(), view.path(), "pull-dir-to-file");
    tick();
    fs::write(view.path().join("dir/new.txt"), b"ours\n").unwrap();
    fs::write(view.path().join("dir/sub/new.txt"), b"ours\n").unwrap();
    fs::remove_dir_all(base.path().join("dir")).unwrap();
    fs::write(base.path().join("dir"), b"now a file\n").unwrap();
    let before = tree_facts(view.path());

    // The pull takes the base version, as for any other conflict, and says so.
    let plan = build_plan(&ws).unwrap();
    assert_eq!(tree_facts(view.path()), before);
    assert_eq!(plan.conflicts, [Path::new("dir")]);
    assert!(plan.kept.is_empty(), "{:?}", plan.kept);
}

#[test]
#[serial]
fn journal_lookup_matches_the_exact_workspace_name() {
    // DATA-02: an open journal of `a-b` matched `a` by file-name prefix, so `a` was blocked
    // and `restore a --resume` applied `a-b`'s steps.
    use forkspace::changes::{ChangeKind, FileChange};
    use forkspace::sync::journal::{self, OperationJournal, OperationKind, OperationPhase};

    let _home = common::setup_test_env();
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::write(source.path().join("f"), b"a-b's file\n").unwrap();
    for op in [
        OperationKind::Merge,
        OperationKind::Pull,
        OperationKind::Restore,
    ] {
        let path = journal::path(op, "a-b", 7);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let open = OperationJournal {
            op,
            workspace: "a-b".to_string(),
            source: source.path().to_path_buf(),
            target: target.path().to_path_buf(),
            changes: vec![FileChange::new("f".into(), ChangeKind::Added)],
            next: 0,
            phase: OperationPhase::Applying,
            fork_time_ns: 7,
            merged: Vec::new(),
            undo: None,
            guard: None,
            held: Vec::new(),
            installed: Vec::new(),
            base_delta: Vec::new(),
            earlier: Vec::new(),
            deferred: Vec::new(),
        };
        journal::save(&path, &open).unwrap();
        assert_eq!(journal::find_open(op, "a").unwrap(), None, "{op:?}");
        assert_eq!(journal::find_open(op, "a-b").unwrap(), Some(path), "{op:?}");
    }
    // A name that ends like a fork time is still compared whole.
    assert_eq!(
        journal::find_open(OperationKind::Pull, "a-b-7").unwrap(),
        None
    );
    // The file name and the recorded owner must both say `a`: each check alone stops a
    // journal whose other half names `a-b`.
    let merge_ab = journal::path(OperationKind::Merge, "a-b", 7);
    let mut misnamed = journal::load(&merge_ab).unwrap();
    misnamed.workspace = "a".to_string();
    journal::save(&merge_ab, &misnamed).unwrap();
    assert_eq!(journal::find_open(OperationKind::Merge, "a").unwrap(), None);
    misnamed.workspace = "a-b".to_string();
    journal::save(&merge_ab, &misnamed).unwrap();
    let copied = journal::path(OperationKind::Pull, "a", 8);
    fs::copy(journal::path(OperationKind::Pull, "a-b", 7), &copied).unwrap();
    assert_eq!(journal::find_open(OperationKind::Pull, "a").unwrap(), None);
    fs::remove_file(copied).unwrap();
    forkspace::workspace::guard::ensure_idle("a").unwrap();
    assert!(forkspace::workspace::guard::ensure_idle("a-b").is_err());

    let mut a = Workspace::new("a", source.path().to_path_buf(), None);
    a.view_path = target.path().to_path_buf();
    assert!(!forkspace::sync::checkpoint::resume(&a).unwrap());
    assert!(pull::resume_apply(&a).is_err());
    assert!(
        !target.path().join("f").exists(),
        "applied another workspace's journal"
    );
}

#[test]
#[serial]
fn convergent_edit_is_a_noop_but_still_rebaselines() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "same", b"old\n");
    let mut ws = workspace(base.path(), view.path(), "pull-convergent");
    tick();
    fs::write(base.path().join("same"), b"same-new\n").unwrap();
    fs::write(view.path().join("same"), b"same-new\n").unwrap();
    let plan = build_plan(&ws).unwrap();
    assert!(plan.changes.is_empty());
    assert_eq!(plan.convergent, [std::path::PathBuf::from("same")]);
    let journal = pull::start(&ws, &plan).unwrap();
    pull::finish(&mut ws, &journal).unwrap();
    pull::complete(&journal).unwrap();
    assert!(collect_clone_changes(&ws, true, false).unwrap().is_empty());
    assert_eq!(ws.manifest_generation, 2);
}

#[test]
#[serial]
fn pull_json_prints_the_plan_it_applies() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("file"), b"old\n").unwrap();
    let ws = forkspace::workspace::create(
        base.path().to_path_buf(),
        "pull-json",
        forkspace::workspace::CreateOptions::default(),
    )
    .unwrap();
    fs::write(base.path().join("file"), b"new\n").unwrap();

    let json = forkspace_command(home.path(), &["pull", "pull-json", "--json"]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["changes"][0]["path"], "file");
    assert_eq!(fs::read(ws.view_path.join("file")).unwrap(), b"new\n");
}

#[test]
#[serial]
fn package_pull_runs_the_manager_in_the_view_and_never_copies_node_modules() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("package.json"), b"{\"name\":\"p\"}\n").unwrap();
    fs::write(
        base.path().join("package-lock.json"),
        b"{\"lockfileVersion\":3,\"packages\":{}}\n",
    )
    .unwrap();
    let ws = forkspace::workspace::create(
        base.path().to_path_buf(),
        "pull-package",
        forkspace::workspace::CreateOptions {
            ..Default::default()
        },
    )
    .unwrap();
    fs::write(
        base.path().join("package.json"),
        b"{\"name\":\"p\",\"dependencies\":{\"zod\":\"1\"}}\n",
    )
    .unwrap();
    fs::write(
        base.path().join("package-lock.json"),
        b"{\"lockfileVersion\":3,\"packages\":{\"node_modules/zod\":{}}}\n",
    )
    .unwrap();
    fs::create_dir_all(base.path().join("node_modules/zod")).unwrap();
    fs::write(
        base.path().join("node_modules/zod/index.js"),
        b"module.exports=1",
    )
    .unwrap();

    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("npm-cwd");
    fs::write(
        tools.path().join("npm"),
        format!("#!/bin/sh\npwd > '{}'\n", log.display()),
    )
    .unwrap();
    let mut permissions = fs::metadata(tools.path().join("npm"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(tools.path().join("npm"), permissions).unwrap();
    let path = format!(
        "{}:{}",
        tools.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .env("PATH", path)
        .args(["pull", "pull-package"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // `pwd` prints the physical path (macOS temporary directories sit under `/private`).
    assert_eq!(
        Path::new(fs::read_to_string(log).unwrap().trim()),
        ws.view_path.canonicalize().unwrap()
    );
    assert!(!ws.view_path.join("node_modules/zod/index.js").exists());
    assert!(fs::read_to_string(ws.view_path.join("package.json"))
        .unwrap()
        .contains("zod"));
}

#[test]
#[serial]
fn merge_after_pull_pushes_only_remaining_workspace_edits() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "advanced", b"old\n");
    write_pair(base.path(), view.path(), "ours-a", b"old\n");
    write_pair(base.path(), view.path(), "ours-b", b"old\n");
    let mut ws = workspace(base.path(), view.path(), "pull-then-merge");
    fs::write(base.path().join("advanced"), b"base\n").unwrap();
    let journal = pull::start(&ws, &build_plan(&ws).unwrap()).unwrap();
    pull::finish(&mut ws, &journal).unwrap();
    pull::complete(&journal).unwrap();
    fs::write(view.path().join("ours-a"), b"a\n").unwrap();
    fs::write(view.path().join("ours-b"), b"b\n").unwrap();
    let plan = forkspace::sync::plan::build_from_changes(
        &ws,
        ws.view_path.clone(),
        collect_clone_changes(&ws, true, false).unwrap(),
        false,
    )
    .unwrap();
    assert_eq!(
        plan.changes
            .iter()
            .map(|change| change.path.as_path())
            .collect::<Vec<_>>(),
        [Path::new("ours-a"), Path::new("ours-b")]
    );
    let journal = forkspace::sync::merge::execute(&plan).unwrap();
    forkspace::sync::journal::remove(&journal).unwrap();
    assert_eq!(fs::read(base.path().join("advanced")).unwrap(), b"base\n");
}

#[test]
#[serial]
fn child_pull_uses_its_immediate_parent_and_propagates_one_level_at_a_time() {
    let _home = common::setup_test_env();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("file"), b"root-0\n").unwrap();
    fs::write(root.path().join("root-only"), b"root-0\n").unwrap();
    let parent = forkspace::workspace::create(
        root.path().to_path_buf(),
        "pull-parent",
        forkspace::workspace::CreateOptions {
            ..Default::default()
        },
    )
    .unwrap();
    let child = forkspace::workspace::create(
        std::path::PathBuf::from("pull-parent"),
        "pull-child",
        forkspace::workspace::CreateOptions {
            ..Default::default()
        },
    )
    .unwrap();
    tick();
    fs::write(parent.view_path.join("file"), b"parent-1\n").unwrap();
    pull_via_state("pull-child");
    assert_eq!(
        fs::read(child.view_path.join("file")).unwrap(),
        b"parent-1\n"
    );

    fs::write(root.path().join("root-only"), b"root-2\n").unwrap();
    assert_eq!(
        fs::read(child.view_path.join("file")).unwrap(),
        b"parent-1\n"
    );
    pull_via_state("pull-parent");
    assert_eq!(
        fs::read(child.view_path.join("root-only")).unwrap(),
        b"root-0\n"
    );
    pull_via_state("pull-child");
    assert_eq!(
        fs::read(child.view_path.join("root-only")).unwrap(),
        b"root-2\n"
    );
}

#[test]
#[serial]
fn degraded_missing_and_interrupted_ancestors_refuse_with_actionable_remedies() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "file", b"old\n");
    let mut ws = workspace(base.path(), view.path(), "pull-degraded");
    ws.backend_kind = BackendKind::PlainCopy;
    ws.backend_state = BackendState::CowDir {
        reflinked_files: 0,
        copied_files: 1,
        unknown_fields: serde_json::Map::new(),
    };
    ws.manifest_status = ManifestStatus::Building;
    let error = pull::build(&ws).unwrap_err().to_string();
    assert!(error.contains("no ready fork-time manifest"));
    ws.manifest_status = ManifestStatus::Absent;
    let error = pull::build(&ws).unwrap_err().to_string();
    assert!(error.contains("no ready fork-time manifest"));
    ws.manifest_status = ManifestStatus::Ready;
    fs::remove_dir_all(base.path()).unwrap();
    let error = pull::build(&ws).unwrap_err().to_string();
    assert!(error.contains("base directory"));
}

#[test]
fn zsh_completion_exposes_every_pull_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("--json"), "missing --json");
}

fn forkspace_command(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home)
        .args(args)
        .output()
        .unwrap()
}

fn pull_via_state(name: &str) {
    tick();
    forkspace::state::State::with_state(|state| {
        let mut workspace = state.require_workspace(name)?.clone();
        let plan = pull::build(&workspace)?;
        let journal = pull::start(&workspace, &plan)?;
        pull::finish(&mut workspace, &journal)?;
        pull::complete(&journal)?;
        *state.require_workspace_mut(name)? = workspace;
        Ok(())
    })
    .unwrap();
}

fn tree_facts(root: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut facts = Vec::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.unwrap();
        if entry.file_type().is_dir() {
            continue;
        }
        let path = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .as_os_str()
            .as_encoded_bytes()
            .to_vec();
        let bytes = if entry.file_type().is_symlink() {
            fs::read_link(entry.path())
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else {
            fs::read(entry.path()).unwrap()
        };
        facts.push((path, bytes));
    }
    facts.sort();
    facts
}

/// Rebuild the manifest of `ws` after the view was written, as the background walk does
/// when the workspace writes before it gets there: those rows are dirty.
fn rebuild_manifest_late(ws: &Workspace, view: &Path) {
    manifest::build(&ws.name, view, ws.fork_time_ns).unwrap();
}

fn paths(changes: &[forkspace::changes::FileChange]) -> Vec<String> {
    changes
        .iter()
        .map(|change| change.path.display().to_string())
        .collect()
}

#[test]
#[serial]
fn dirty_rows_are_workspace_writes_not_base_changes() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "a.txt", b"v0\n");
    write_pair(base.path(), view.path(), "b.bin", b"bin\0ary0");
    let ws = workspace(base.path(), view.path(), "pull-dirty");
    forkspace::sync::ancestor::record(&ws.name, base.path()).unwrap();
    tick();
    fs::write(view.path().join("a.txt"), b"v1 agent\n").unwrap();
    fs::write(view.path().join("b.bin"), b"bin\0ary1 agent").unwrap();
    fs::write(view.path().join("new.txt"), b"agent created\n").unwrap();
    rebuild_manifest_late(&ws, view.path());
    fs::write(base.path().join("c.txt"), b"base\n").unwrap();

    assert_eq!(paths(&pull::base_changes(&ws).unwrap()), ["c.txt"]);
    let plan = build_plan(&ws).unwrap();
    assert_eq!(paths(&plan.changes), ["c.txt"]);
    assert!(plan.conflicts.is_empty() && plan.auto_merged.is_empty());
}

#[test]
#[serial]
fn a_base_file_touched_without_changing_bytes_is_not_a_base_change() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "logo.bin", b"\x89PNG\0fork");
    write_pair(base.path(), view.path(), "src/old.ts", b"one\n");
    let ws = workspace(base.path(), view.path(), "pull-touch");
    forkspace::sync::ancestor::record(&ws.name, base.path()).unwrap();
    tick();
    fs::write(view.path().join("logo.bin"), b"\x89PNG\0agent").unwrap();
    fs::remove_file(view.path().join("src/old.ts")).unwrap();
    // A checkout round trip, a rebase, `touch`: same bytes, new mtime.
    for path in ["logo.bin", "src/old.ts"] {
        fs::OpenOptions::new()
            .write(true)
            .open(base.path().join(path))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
            .unwrap();
    }

    assert!(pull::base_changes(&ws).unwrap().is_empty());
    let plan = build_plan(&ws).unwrap();
    assert!(plan.changes.is_empty() && plan.conflicts.is_empty());
}

#[test]
#[serial]
fn a_base_rewrite_that_keeps_size_and_mtime_is_a_base_change() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "x", b"one\ntwo\nthree\n");
    let ws = workspace(base.path(), view.path(), "pull-same-size");
    forkspace::sync::ancestor::record(&ws.name, base.path()).unwrap();
    tick();
    // `rsync -a`, `touch -r`, an in-place rewrite: same size, mtime put back.
    let target = base.path().join("x");
    let mtime = fs::metadata(&target).unwrap().modified().unwrap();
    fs::write(&target, b"one\ntwo\nTHREE\n").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&target)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();

    assert_eq!(paths(&pull::base_changes(&ws).unwrap()), ["x"]);
}

#[test]
#[serial]
fn a_dirty_row_is_no_base_change_even_without_a_fork_time_copy() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "a.txt", b"v0\n");
    let ws = workspace(base.path(), view.path(), "pull-dirty-no-copy");
    tick();
    fs::write(view.path().join("a.txt"), b"v1 agent\n").unwrap();
    rebuild_manifest_late(&ws, view.path());
    fs::write(base.path().join("c.txt"), b"base\n").unwrap();

    // The base never wrote a.txt after the fork, so its row (recorded from the view) says
    // nothing about the base.
    assert_eq!(paths(&pull::base_changes(&ws).unwrap()), ["c.txt"]);
}
