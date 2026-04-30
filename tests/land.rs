mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use forkspace::backend::{BackendKind, BackendState};
use forkspace::state::State;
use forkspace::workspace::Workspace;
use serial_test::serial;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(path: &Path, message: &str, content: &str) {
    fs::write(path.join("file"), content).unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-m", message]);
}

struct Fixture {
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    base: std::path::PathBuf,
    view: std::path::PathBuf,
}

impl Fixture {
    fn new(branch: Option<&str>) -> Self {
        let home = common::setup_test_env();
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("base");
        let view = root.path().join("view");
        fs::create_dir(&base).unwrap();
        git(&base, &["init", "--quiet", "--initial-branch=main"]);
        git(
            &base,
            &["config", "user.email", "forkspace@example.invalid"],
        );
        git(&base, &["config", "user.name", "Forkspace Tests"]);
        commit(&base, "initial", "initial\n");
        let status = Command::new("git")
            .args(["clone", "--quiet", "--no-hardlinks"])
            .arg(&base)
            .arg(&view)
            .status()
            .unwrap();
        assert!(status.success());
        git(
            &view,
            &["config", "user.email", "forkspace@example.invalid"],
        );
        git(&view, &["config", "user.name", "Forkspace Tests"]);
        if let Some(branch) = branch {
            git(&view, &["switch", "-c", branch]);
        }
        let mut workspace = Workspace::new("attempt", base.clone(), None);
        workspace.backend_kind = BackendKind::PlainCopy;
        workspace.backend_state = BackendState::CowDir {
            reflinked_files: 0,
            copied_files: 1,
            unknown_fields: serde_json::Map::new(),
        };
        workspace.view_path = view.clone();
        workspace.branch = branch.map(str::to_string);
        let mut state = State::default();
        state.add_workspace(workspace).unwrap();
        state.save().unwrap();
        Self {
            _home: home,
            _root: root,
            base,
            view,
        }
    }

    fn land(&self, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .arg("land")
            .arg("attempt")
            .args(extra)
            .env(
                "FORKSPACE_HOME",
                std::env::var_os("FORKSPACE_HOME").unwrap(),
            )
            .output()
            .unwrap()
    }
}

#[test]
#[serial]
fn public_fork_branch_then_land_preserves_the_workspace_commits() {
    let home = common::setup_test_env();
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("base");
    fs::create_dir(&base).unwrap();
    git(&base, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &base,
        &["config", "user.email", "forkspace@example.invalid"],
    );
    git(&base, &["config", "user.name", "Forkspace Tests"]);
    commit(&base, "initial", "initial\n");

    let forked = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args([
            "fork",
            base.to_str().unwrap(),
            "--name",
            "public",
            "--branch",
            "feature/public",
            "--copy",
        ])
        .output()
        .unwrap();
    assert!(
        forked.status.success(),
        "{}",
        String::from_utf8_lossy(&forked.stderr)
    );
    let state = State::load().unwrap();
    let view = state.require_workspace("public").unwrap().view_path.clone();
    assert_eq!(git(&view, &["branch", "--show-current"]), "feature/public");
    commit(&view, "workspace one", "one\n");
    commit(&view, "workspace two", "two\n");
    let expected = git(&view, &["rev-parse", "HEAD"]);

    let landed = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args(["land", "public"])
        .output()
        .unwrap();
    assert!(
        landed.status.success(),
        "{}",
        String::from_utf8_lossy(&landed.stderr)
    );
    assert_eq!(
        git(&base, &["rev-parse", "refs/heads/feature/public"]),
        expected
    );
    assert_eq!(
        git(&base, &["rev-list", "--count", "main..feature/public"]),
        "2"
    );
}

#[test]
#[serial]
fn land_preserves_commit_hashes_and_fast_forwards_again() {
    let fixture = Fixture::new(Some("feature"));
    commit(&fixture.view, "one", "one\n");
    commit(&fixture.view, "two", "two\n");
    commit(&fixture.view, "three", "three\n");
    let tip = git(&fixture.view, &["rev-parse", "HEAD"]);
    let output = fixture.land(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git(&fixture.base, &["rev-parse", "feature"]), tip);
    assert_eq!(
        git(&fixture.base, &["rev-list", "--count", "main..feature"]),
        "3"
    );

    commit(&fixture.view, "four", "four\n");
    commit(&fixture.view, "five", "five\n");
    let tip = git(&fixture.view, &["rev-parse", "HEAD"]);
    let output = fixture.land(&[]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 new commits"));
    assert_eq!(git(&fixture.base, &["rev-parse", "feature"]), tip);
}

#[test]
#[serial]
fn killed_after_fetch_leaves_no_public_ref_and_the_next_land_converges() {
    let fixture = Fixture::new(Some("feature"));
    commit(&fixture.view, "workspace", "workspace\n");
    let expected = git(&fixture.view, &["rev-parse", "HEAD"]);
    let tools = tempfile::tempdir().unwrap();
    let marker = tools.path().join("fetched");
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    assert!(real_git.status.success());
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let wrapper = tools.path().join("git");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n{git} \"$@\"\nstatus=$?\ncase \" $* \" in *\" fetch \"*) : > \"$FORKSPACE_TEST_FETCH_MARKER\"; sleep 30;; esac\nexit $status\n",
            git = real_git.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        tools.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env(
            "FORKSPACE_HOME",
            std::env::var_os("FORKSPACE_HOME").unwrap(),
        )
        .env("FORKSPACE_TEST_FETCH_MARKER", &marker)
        .env("PATH", path)
        .args(["land", "attempt"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        marker.exists(),
        "git fetch did not reach the fault boundary"
    );
    child.kill().unwrap();
    let _ = child.wait();

    assert!(!forkspace::git::ref_exists(
        &fixture.base,
        "refs/heads/feature"
    ));
    assert!(forkspace::git::ref_exists(
        &fixture.base,
        "refs/forkspace/land/attempt"
    ));
    let recovered = fixture.land(&[]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(git(&fixture.base, &["rev-parse", "feature"]), expected);
    assert!(git(&fixture.base, &["for-each-ref", "refs/forkspace/land/"]).is_empty());
}

#[test]
#[serial]
fn land_refuses_dirty_and_divergent_history_unless_forced() {
    let fixture = Fixture::new(Some("feature"));
    commit(&fixture.view, "workspace", "workspace\n");
    fs::write(fixture.view.join("file"), "dirty\n").unwrap();
    let dirty = fixture.land(&[]);
    assert_eq!(dirty.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&dirty.stderr).contains("forkspace merge"));
    git(&fixture.view, &["restore", "file"]);
    assert!(fixture.land(&[]).status.success());

    git(&fixture.base, &["switch", "feature"]);
    commit(&fixture.base, "base-only", "base-only\n");
    git(&fixture.base, &["switch", "main"]);
    commit(&fixture.view, "workspace-only", "workspace-only\n");
    let refused = fixture.land(&[]);
    assert_eq!(refused.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("base-only"));
    assert!(fixture.land(&["--force"]).status.success());
    assert_eq!(
        git(&fixture.base, &["rev-parse", "feature"]),
        git(&fixture.view, &["rev-parse", "feature"])
    );
    assert!(git(&fixture.base, &["for-each-ref", "refs/forkspace/land/"]).is_empty());
}

#[test]
#[serial]
fn default_branch_requires_as_and_branch_preflight_is_clone_free() {
    let fixture = Fixture::new(None);
    commit(&fixture.view, "main-work", "main-work\n");
    let refused = fixture.land(&[]);
    assert_eq!(refused.status.code(), Some(6));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("default branch"));
    assert!(fixture.land(&["--as", "topic"]).status.success());
    assert_eq!(
        git(&fixture.base, &["rev-parse", "topic"]),
        git(&fixture.view, &["rev-parse", "HEAD"])
    );

    let non_repo = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .args(["fork", "--name", "never-created", "--branch", "topic"])
        .arg(non_repo.path())
        .env(
            "FORKSPACE_HOME",
            std::env::var_os("FORKSPACE_HOME").unwrap(),
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert!(!State::load()
        .unwrap()
        .workspaces
        .contains_key("never-created"));
}

#[test]
#[serial]
fn checked_out_missing_base_untracked_and_drop_guards_preserve_recoverability() {
    let fixture = Fixture::new(Some("feature"));
    commit(&fixture.view, "workspace", "workspace\n");
    git(&fixture.base, &["branch", "feature"]);
    git(&fixture.base, &["switch", "feature"]);
    let checked_out = fixture.land(&[]);
    assert_eq!(checked_out.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&checked_out.stderr).contains("git switch -"));
    assert!(git(&fixture.base, &["for-each-ref", "refs/forkspace/land/"]).is_empty());
    git(&fixture.base, &["switch", "main"]);
    git(&fixture.base, &["branch", "-D", "feature"]);

    fs::write(fixture.view.join("untracked.tmp"), b"scratch\n").unwrap();
    let landed = fixture.land(&[]);
    assert!(landed.status.success());
    assert!(String::from_utf8_lossy(&landed.stderr).contains("untracked files"));

    commit(&fixture.view, "again", "again\n");
    fs::write(fixture.view.join("file"), b"dirty\n").unwrap();
    let failed_drop = fixture.land(&["--drop"]);
    assert_eq!(failed_drop.status.code(), Some(5));
    assert!(State::load().unwrap().workspaces.contains_key("attempt"));
    git(&fixture.view, &["restore", "file"]);
    let expected_tip = git(&fixture.view, &["rev-parse", "feature"]);
    let dropped = fixture.land(&["--drop"]);
    assert!(dropped.status.success());
    assert!(!State::load().unwrap().workspaces.contains_key("attempt"));
    assert_eq!(git(&fixture.base, &["rev-parse", "feature"]), expected_tip);
}

#[test]
#[serial]
fn moved_base_is_not_found_and_the_independent_view_survives() {
    let fixture = Fixture::new(Some("feature"));
    commit(&fixture.view, "workspace", "workspace\n");
    let moved = fixture.base.with_file_name("base-moved");
    fs::rename(&fixture.base, &moved).unwrap();
    let output = fixture.land(&[]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains(&fixture.base.display().to_string()));
    assert!(fixture.view.is_dir());
    assert_eq!(git(&fixture.view, &["status", "--porcelain"]), "");
}

#[test]
#[serial]
fn stacked_child_lands_every_commit_into_the_root_repository() {
    let home = common::setup_test_env();
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("base");
    let parent_view = root.path().join("parent-view");
    let child_view = root.path().join("child-view");
    fs::create_dir(&base).unwrap();
    git(&base, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &base,
        &["config", "user.email", "forkspace@example.invalid"],
    );
    git(&base, &["config", "user.name", "Forkspace Tests"]);
    commit(&base, "initial", "initial\n");
    assert!(Command::new("git")
        .args(["clone", "--quiet", "--no-hardlinks"])
        .arg(&base)
        .arg(&parent_view)
        .status()
        .unwrap()
        .success());
    git(
        &parent_view,
        &["config", "user.email", "forkspace@example.invalid"],
    );
    git(&parent_view, &["config", "user.name", "Forkspace Tests"]);
    git(&parent_view, &["switch", "-c", "feat"]);
    commit(&parent_view, "parent", "parent\n");
    assert!(Command::new("git")
        .args(["clone", "--quiet", "--no-hardlinks"])
        .arg(&parent_view)
        .arg(&child_view)
        .status()
        .unwrap()
        .success());
    git(
        &child_view,
        &["config", "user.email", "forkspace@example.invalid"],
    );
    git(&child_view, &["config", "user.name", "Forkspace Tests"]);
    git(&child_view, &["switch", "-c", "feat-2"]);
    commit(&child_view, "child", "child\n");

    let mut parent = Workspace::new("parent", base.clone(), None);
    parent.backend_kind = BackendKind::MacClone;
    parent.backend_state = BackendState::MacClone {
        unknown_fields: serde_json::Map::new(),
    };
    parent.view_path = parent_view.clone();
    parent.branch = Some("feat".to_string());
    let mut child = Workspace::new("child", parent_view.clone(), Some("parent".to_string()));
    child.backend_kind = BackendKind::MacClone;
    child.backend_state = BackendState::MacClone {
        unknown_fields: serde_json::Map::new(),
    };
    child.view_path = child_view.clone();
    child.branch = Some("feat-2".to_string());
    let mut state = State::default();
    state.add_workspace(parent).unwrap();
    state.add_workspace(child).unwrap();
    state.save().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .env("GIT_ALLOW_PROTOCOL", "file")
        .args(["land", "child"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git(&base, &["rev-list", "--count", "main..feat-2"]), "2");
    assert_eq!(
        git(&base, &["rev-parse", "feat-2"]),
        git(&child_view, &["rev-parse", "feat-2"])
    );
    assert!(!forkspace::git::ref_exists(
        &parent_view,
        "refs/heads/feat-2"
    ));
}

#[test]
#[serial]
fn branch_preflight_refuses_a_git_worktree_without_touching_the_repository() {
    let home = common::setup_test_env();
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    let worktree = root.path().join("worktree");
    fs::create_dir(&main).unwrap();
    git(&main, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &main,
        &["config", "user.email", "forkspace@example.invalid"],
    );
    git(&main, &["config", "user.name", "Forkspace Tests"]);
    commit(&main, "initial", "initial\n");
    git(
        &main,
        &["worktree", "add", "--quiet", worktree.to_str().unwrap()],
    );
    let refs_before = git(&main, &["show-ref"]);
    let objects_before = git(&main, &["count-objects", "-v"]);
    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env("FORKSPACE_HOME", home.path())
        .args([
            "fork",
            worktree.to_str().unwrap(),
            "--name",
            "no-worktree",
            "--branch",
            "topic",
            "--copy",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert!(String::from_utf8_lossy(&output.stderr).contains("main checkout"));
    assert_eq!(git(&main, &["show-ref"]), refs_before);
    assert_eq!(git(&main, &["count-objects", "-v"]), objects_before);
    assert!(!State::load()
        .unwrap()
        .workspaces
        .contains_key("no-worktree"));
}

#[test]
#[serial]
fn land_is_a_local_file_protocol_fetch_and_preserves_commit_metadata() {
    let fixture = Fixture::new(Some("feature"));
    git(&fixture.view, &["remote", "remove", "origin"]);
    commit(&fixture.view, "one", "one\n");
    commit(&fixture.view, "two", "two\n");
    commit(&fixture.view, "three", "three\n");
    let metadata = git(
        &fixture.view,
        &[
            "log",
            "--format=%H%x09%an%x09%ae%x09%aI%x09%s",
            "main..feature",
        ],
    );
    let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
        .env(
            "FORKSPACE_HOME",
            std::env::var_os("FORKSPACE_HOME").unwrap(),
        )
        .env("GIT_ALLOW_PROTOCOL", "file")
        .args(["land", "attempt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        git(
            &fixture.base,
            &[
                "log",
                "--format=%H%x09%an%x09%ae%x09%aI%x09%s",
                "main..feature"
            ]
        ),
        metadata
    );
}

#[test]
#[serial]
fn lfs_pointers_and_changed_submodule_commits_fail_loudly() {
    let fixture = Fixture::new(Some("feature"));
    fs::write(
        fixture.view.join("large.bin"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nsize 42\n",
    )
    .unwrap();
    git(&fixture.view, &["add", "large.bin"]);
    git(&fixture.view, &["commit", "-m", "lfs pointer"]);
    let lfs = fixture.land(&[]);
    assert_eq!(lfs.status.code(), Some(6));
    let stderr = String::from_utf8_lossy(&lfs.stderr);
    assert!(stderr.contains("Git LFS pointer large.bin"));
    assert!(stderr.contains("publish Git LFS objects"));
    assert!(!forkspace::git::ref_exists(
        &fixture.base,
        "refs/heads/feature"
    ));

    git(&fixture.view, &["reset", "--hard", "HEAD~1"]);
    fs::create_dir_all(fixture.view.join("vendor")).unwrap();
    let cloned = Command::new("git")
        .args(["clone", "--quiet"])
        .arg(&fixture.view)
        .arg(fixture.view.join("vendor/dependency"))
        .status()
        .unwrap();
    assert!(cloned.success());
    git(&fixture.view, &["add", "vendor/dependency"]);
    git(&fixture.view, &["commit", "-m", "submodule pointer"]);
    let submodule = fixture.land(&[]);
    assert_eq!(submodule.status.code(), Some(6));
    let stderr = String::from_utf8_lossy(&submodule.stderr);
    assert!(stderr.contains("submodule vendor/dependency"));
    assert!(stderr.contains("submodule commits"));
    assert!(!forkspace::git::ref_exists(
        &fixture.base,
        "refs/heads/feature"
    ));
}
