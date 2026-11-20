//! `merge` keeps the commits a workspace made as a branch of its project, named after the
//! workspace, without touching the project's checked-out branch; `merge --undo` takes that
//! branch back with the files.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use common::cli::{stderr, stdout, write, Env};
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

fn commit(path: &Path, file: &str, content: &str, message: &str) {
    fs::write(path.join(file), content).unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "--quiet", "-m", message]);
}

/// A Git project on `main` with one commit.
fn repository(env: &Env) {
    let project = env.project();
    git(&project, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &project,
        &["config", "user.email", "forkspace@example.invalid"],
    );
    git(&project, &["config", "user.name", "Forkspace Tests"]);
    commit(&project, "file", "initial\n", "initial");
}

fn branch_exists(project: &Path, branch: &str) -> bool {
    !git(project, &["branch", "--list", branch]).is_empty()
}

#[test]
#[serial]
fn commits_are_kept_as_a_branch_and_the_project_branch_is_left_alone() {
    let env = Env::new();
    repository(&env);
    let project = env.project();
    let main_before = git(&project, &["rev-parse", "main"]);
    env.ok(&["new", "w"]);
    let view = env.view("w");
    git(&view, &["config", "user.email", "agent@example.invalid"]);
    git(&view, &["config", "user.name", "Agent"]);
    commit(&view, "file", "one\n", "agent one");
    commit(&view, "file", "two\n", "agent two");
    let tip = git(&view, &["rev-parse", "HEAD"]);

    let merged = env.ok(&["merge", "w"]);
    assert!(
        stdout(&merged).contains("kept the workspace's commits as branch 'w'"),
        "{}",
        stdout(&merged)
    );
    assert_eq!(
        git(&project, &["rev-parse", "w"]),
        tip,
        "the commits are kept as they are"
    );
    assert_eq!(git(&project, &["rev-list", "--count", "main..w"]), "2");
    // The project's own branch did not move; the files arrive as uncommitted edits.
    assert_eq!(git(&project, &["branch", "--show-current"]), "main");
    assert_eq!(git(&project, &["rev-parse", "main"]), main_before);
    assert_eq!(fs::read_to_string(project.join("file")).unwrap(), "two\n");
    assert_eq!(git(&project, &["status", "--porcelain"]), "M file");

    // More commits, after the workspace caught up with the project, move the branch forward.
    env.ok(&["pull", "w"]);
    commit(&view, "file", "three\n", "agent three");
    env.ok(&["merge", "w"]);
    assert_eq!(git(&project, &["rev-list", "--count", "main..w"]), "3");
}

#[test]
#[serial]
fn a_project_branch_that_went_another_way_is_never_moved() {
    let env = Env::new();
    repository(&env);
    let project = env.project();
    env.ok(&["new", "w"]);
    let view = env.view("w");
    git(&view, &["config", "user.email", "agent@example.invalid"]);
    git(&view, &["config", "user.name", "Agent"]);
    commit(&view, "agent.txt", "agent\n", "agent work");
    // Someone made a branch called `w` in the project after the fork.
    git(&project, &["branch", "w"]);
    git(&project, &["switch", "--quiet", "w"]);
    commit(&project, "theirs.txt", "theirs\n", "their work");
    git(&project, &["switch", "--quiet", "main"]);
    let theirs = git(&project, &["rev-parse", "w"]);

    let merged = env.ok(&["merge", "w"]);
    assert!(
        stdout(&merged).contains("as branch 'w-2'"),
        "{}",
        stdout(&merged)
    );
    assert_eq!(git(&project, &["rev-parse", "w"]), theirs);
    assert_eq!(
        git(&project, &["log", "-1", "--format=%s", "w-2"]),
        "agent work"
    );
}

#[test]
#[serial]
fn a_workspace_without_commits_adds_no_branch_and_undo_takes_one_back() {
    let env = Env::new();
    repository(&env);
    let project = env.project();
    env.ok(&["new", "quiet"]);
    write(&env.view("quiet").join("file"), "edited\n");
    env.ok(&["merge", "quiet"]);
    assert!(!branch_exists(&project, "quiet"));

    env.ok(&["new", "busy"]);
    let view = env.view("busy");
    git(&view, &["config", "user.email", "agent@example.invalid"]);
    git(&view, &["config", "user.name", "Agent"]);
    commit(&view, "busy.txt", "busy\n", "busy work");
    env.ok(&["merge", "busy"]);
    assert!(branch_exists(&project, "busy"));
    let undone = env.ok(&["merge", "--undo", "busy"]);
    assert!(
        stdout(&undone).contains("branch 'busy'"),
        "{}",
        stdout(&undone)
    );
    assert!(!branch_exists(&project, "busy"));
    assert!(!project.join("busy.txt").exists());
}

#[test]
#[serial]
fn dropping_unmerged_commits_says_they_go() {
    let env = Env::new();
    repository(&env);
    env.ok(&["new", "w"]);
    let view = env.view("w");
    git(&view, &["config", "user.email", "agent@example.invalid"]);
    git(&view, &["config", "user.name", "Agent"]);
    commit(&view, "a.txt", "a\n", "agent work");
    let dropped = env.ok(&["drop", "w"]);
    assert!(
        stderr(&dropped).contains("never merged"),
        "{}",
        stderr(&dropped)
    );
}

#[test]
#[serial]
fn a_git_worktree_base_gets_a_private_repository() {
    let home = common::setup_test_env();
    let forkspace = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_forkspace"))
            .env("FORKSPACE_HOME", home.path())
            .args(args)
            .output()
            .unwrap()
    };
    let stderr = |output: &Output| String::from_utf8_lossy(&output.stderr).into_owned();
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
    commit(&main, "file", "initial\n", "initial");
    git(
        &main,
        &["worktree", "add", "--quiet", worktree.to_str().unwrap()],
    );
    let refs_before = git(&main, &["show-ref"]);
    let worktree_head = git(&worktree, &["rev-parse", "HEAD"]);
    let worktree_branch = git(&worktree, &["branch", "--show-current"]);

    let created = common::new_in(home.path(), &worktree, &["topic"]);
    assert!(created.status.success(), "{}", stderr(&created));
    assert_eq!(git(&main, &["show-ref"]), refs_before);

    let view = common::view_of(home.path(), "topic");
    assert!(view.join(".git").is_dir());
    fs::write(view.join("tracked.txt"), "agent\n").unwrap();
    git(&view, &["add", "tracked.txt"]);
    git(&view, &["commit", "--quiet", "-m", "agent work"]);

    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), worktree_head);
    assert_eq!(
        git(&worktree, &["branch", "--show-current"]),
        worktree_branch
    );
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    assert_eq!(git(&main, &["show-ref"]), refs_before);

    let merged = forkspace(&["merge", "topic"]);
    assert!(merged.status.success(), "{}", stderr(&merged));
    assert_eq!(
        git(&main, &["log", "-1", "--format=%s", "topic"]).trim(),
        "agent work"
    );
}
