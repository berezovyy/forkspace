mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

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
        workspace.backend_kind = BackendKind::MacClone;
        workspace.backend_state = BackendState::MacClone {
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
