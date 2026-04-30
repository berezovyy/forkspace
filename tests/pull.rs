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
        std::thread::sleep(std::time::Duration::from_millis(2));
        mutate(base.path(), view.path());
        let before = tree_facts(view.path());
        assert!(matches!(
            build_plan(&ws, false),
            Err(ForkspaceError::PullConflict { .. })
        ));
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
fn convergent_edit_is_a_noop_but_still_rebaselines() {
    let _home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    let view = tempfile::tempdir().unwrap();
    write_pair(base.path(), view.path(), "same", b"old\n");
    let mut ws = workspace(base.path(), view.path(), "pull-convergent");
    std::thread::sleep(std::time::Duration::from_millis(2));
    fs::write(base.path().join("same"), b"same-new\n").unwrap();
    fs::write(view.path().join("same"), b"same-new\n").unwrap();
    let plan = build_plan(&ws, false).unwrap();
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
fn dry_run_text_and_json_match_the_subsequent_public_apply() {
    let home = common::setup_test_env();
    let base = tempfile::tempdir().unwrap();
    fs::write(base.path().join("file"), b"old\n").unwrap();
    let ws = forkspace::workspace::create(
        base.path().to_path_buf(),
        "pull-dry",
        forkspace::workspace::CreateOptions {
            force_copy: true,
            ..Default::default()
        },
    )
    .unwrap();
    fs::write(base.path().join("file"), b"new\n").unwrap();
    let before = tree_facts(&ws.view_path);

    let text = forkspace_command(
        home.path(),
        &["pull", "pull-dry", "--dry-run", "--no-install"],
    );
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("update file"));
    assert_eq!(tree_facts(&ws.view_path), before);
    let json = forkspace_command(
        home.path(),
        &["pull", "pull-dry", "--dry-run", "--json", "--no-install"],
    );
    assert!(json.status.success());
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["changes"][0]["path"], "file");
    assert_eq!(tree_facts(&ws.view_path), before);

    let apply = forkspace_command(home.path(), &["pull", "pull-dry", "--no-install"]);
    assert!(apply.status.success());
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
            force_copy: true,
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
    assert_eq!(
        fs::read_to_string(log).unwrap().trim(),
        ws.view_path.display().to_string()
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
    let journal = pull::start(&ws, &build_plan(&ws, false).unwrap()).unwrap();
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
            force_copy: true,
            ..Default::default()
        },
    )
    .unwrap();
    let child = forkspace::workspace::create(
        std::path::PathBuf::from("pull-parent"),
        "pull-child",
        forkspace::workspace::CreateOptions {
            force_copy: true,
            ..Default::default()
        },
    )
    .unwrap();
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
    let error = pull::build(&ws, false).unwrap_err().to_string();
    assert!(error.contains("no ready fork-time manifest"));
    ws.manifest_status = ManifestStatus::Absent;
    let error = pull::build(&ws, false).unwrap_err().to_string();
    assert!(error.contains("no ready fork-time manifest"));
    ws.manifest_status = ManifestStatus::Ready;
    fs::remove_dir_all(base.path()).unwrap();
    let error = pull::build(&ws, false).unwrap_err().to_string();
    assert!(error.contains("base directory"));
}

#[test]
fn zsh_completion_exposes_every_pull_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    for flag in [
        "--dry-run",
        "--theirs",
        "--resume",
        "--no-install",
        "--json",
    ] {
        assert!(text.contains(flag), "missing {flag}");
    }
}

fn forkspace_command(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home)
        .args(args)
        .output()
        .unwrap()
}

fn pull_via_state(name: &str) {
    forkspace::state::State::with_state(|state| {
        let mut workspace = state.require_workspace(name)?.clone();
        let plan = pull::build(&workspace, false)?;
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
