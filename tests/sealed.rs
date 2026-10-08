//! Sealed overlay workspaces: the lower layer is a frozen copy of the base, so nothing done in
//! the base after the fork reaches a workspace until `pull`.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
}

impl Env {
    /// A Git project with `a.txt` and `b.txt` on `main` and a different `a.txt` on `other`.
    fn new() -> Self {
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
        };
        let project = env.project();
        fs::create_dir(&project).unwrap();
        git(&project, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(project.join("a.txt"), "a\n").unwrap();
        fs::write(project.join("b.txt"), "b\n").unwrap();
        git(&project, &["add", "-A"]);
        git(&project, &["commit", "--quiet", "-m", "init"]);
        git(&project, &["switch", "--quiet", "-c", "other"]);
        fs::write(project.join("a.txt"), "other\n").unwrap();
        git(&project, &["commit", "--quiet", "-am", "other"]);
        git(&project, &["switch", "--quiet", "main"]);
        env
    }

    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn view(&self, name: &str) -> PathBuf {
        self.home.path().join(name).join("merged")
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", self.home.path())
            .env("FORKSPACE_BACKEND", "linux")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "forkspace {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn new_workspace(&self, name: &str, extra: &[&str]) {
        let project = self.project();
        let mut args = vec!["new", name, "--from", project.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.run(&args);
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8(self.run(args).stdout).unwrap()
    }

    fn frozen_copies(&self) -> Vec<PathBuf> {
        let root = self.home.path().join("frozen");
        let mut copies: Vec<_> = fs::read_dir(root)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.join("tree").is_dir())
                    .collect()
            })
            .unwrap_or_default();
        copies.sort();
        copies
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", self.home.path())
            .args(["drop", "--all"])
            .output();
    }
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Forkspace Tests",
            "-c",
            "user.email=t@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn branch_switches_and_commits_in_the_base_do_not_reach_the_workspace() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("ws", &[]);
    let view = env.view("ws");

    git(&env.project(), &["switch", "--quiet", "other"]);
    fs::write(env.project().join("b.txt"), "base edit\n").unwrap();
    git(&env.project(), &["commit", "--quiet", "-am", "base commit"]);
    git(&env.project(), &["branch", "base-only"]);

    assert_eq!(fs::read_to_string(view.join("a.txt")).unwrap(), "a\n");
    assert_eq!(fs::read_to_string(view.join("b.txt")).unwrap(), "b\n");
    assert_eq!(git(&view, &["branch", "--show-current"]).trim(), "main");
    assert_eq!(git(&view, &["status", "--porcelain"]), "");
    assert_eq!(git(&view, &["rev-list", "--count", "HEAD"]).trim(), "1");
    assert!(!git(&view, &["branch", "--list"]).contains("base-only"));
    assert_eq!(env.stdout(&["diff", "ws"]).trim(), "clean — no changes");
}

#[test]
fn later_forks_reuse_the_frozen_copy_or_link_unchanged_files() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("one", &[]);
    env.new_workspace("two", &[]);
    assert_eq!(
        env.frozen_copies().len(),
        1,
        "an unchanged base reuses its copy"
    );

    fs::write(env.project().join("b.txt"), "changed\n").unwrap();
    env.new_workspace("three", &[]);
    let copies = env.frozen_copies();
    assert_eq!(copies.len(), 2);
    let inode = |copy: &Path, name: &str| fs::metadata(copy.join("tree").join(name)).unwrap().ino();
    assert_eq!(
        inode(&copies[0], "a.txt"),
        inode(&copies[1], "a.txt"),
        "unchanged file is linked"
    );
    assert_ne!(
        inode(&copies[0], "b.txt"),
        inode(&copies[1], "b.txt"),
        "changed file is copied"
    );
    assert_eq!(
        fs::read_to_string(env.view("three").join("b.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        fs::read_to_string(env.view("one").join("b.txt")).unwrap(),
        "b\n"
    );
}

#[test]
fn merge_then_pull_moves_one_workspace_s_changes_into_another() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("ws1", &[]);
    env.new_workspace("ws2", &[]);

    fs::write(env.view("ws1").join("feature.txt"), "from ws1\n").unwrap();
    env.run(&["merge", "ws1", "--quiet", "--no-install"]);
    assert!(
        !env.view("ws2").join("feature.txt").exists(),
        "updates wait for pull"
    );

    fs::write(env.view("ws2").join("own.txt"), "ws2\n").unwrap();
    env.run(&["pull", "ws2", "--no-install"]);
    assert_eq!(
        fs::read_to_string(env.view("ws2").join("feature.txt")).unwrap(),
        "from ws1\n"
    );
    assert_eq!(env.stdout(&["diff", "ws2", "--files"]).trim(), "own.txt");
}

#[test]
fn a_stacked_workspace_is_sealed_against_its_parent() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("parent", &[]);
    fs::write(env.view("parent").join("before.txt"), "before\n").unwrap();
    env.run(&["new", "child", "--from", "parent"]);
    fs::write(env.view("parent").join("after.txt"), "after\n").unwrap();

    assert!(env.view("child").join("before.txt").is_file());
    assert!(!env.view("child").join("after.txt").exists());
    assert_eq!(env.stdout(&["diff", "child"]).trim(), "clean — no changes");
}

#[test]
fn drop_removes_frozen_copies_no_workspace_uses() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("one", &[]);
    fs::write(env.project().join("b.txt"), "changed\n").unwrap();
    env.new_workspace("two", &[]);
    assert_eq!(env.frozen_copies().len(), 2);

    env.run(&["drop", "one"]);
    assert_eq!(env.frozen_copies().len(), 1);
    env.run(&["drop", "two"]);
    assert!(env.frozen_copies().is_empty());
}

#[test]
fn live_workspaces_mount_the_base_itself() {
    skip_without_overlay!();
    let env = Env::new();
    env.new_workspace("live", &["--live"]);
    // What a live view shows after the base changes is undefined (fuse-overlayfs caches
    // what it has loaded), which is why workspaces are sealed by default; only check that
    // no frozen copy is made and the workspace is labelled.
    assert!(env.frozen_copies().is_empty());
    assert_eq!(
        fs::read_to_string(env.view("live").join("a.txt")).unwrap(),
        "a\n"
    );
    assert!(env.stdout(&["ls"]).contains("linux (live)"));
    let listed: serde_json::Value = serde_json::from_str(&env.stdout(&["ls", "--json"])).unwrap();
    assert_eq!(listed[0]["live"], true);
}
