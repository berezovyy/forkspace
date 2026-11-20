//! What forks leave out (`src/exclude.rs`): Git worktrees nested in the project, Claude Code's
//! `.claude/worktrees/`, and what `.forkspaceexclude` lists. A copied nested worktree names
//! the project's repository by absolute path, so a commit in the workspace moved the user's
//! own branch while `diff` reported no changes.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::cli::{stdout, write, Env};
use serial_test::serial;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with an agent worktree under `.claude/worktrees`, another nested worktree
/// elsewhere, and a `.forkspaceexclude` entry.
fn project(env: &Env) {
    let project = env.project();
    write(&project.join("src/a.txt"), "a\n");
    write(&project.join(".claude/settings.json"), "{}\n");
    write(
        &project.join(".gitignore"),
        ".claude/worktrees/\ntools/wt/\nbig/\n",
    );
    write(&project.join(".forkspaceexclude"), "big/\n");
    write(&project.join("big/blob.bin"), "large\n");
    git(&project, &["init", "-q"]);
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-qm", "init"]);
    git(
        &project,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent",
            ".claude/worktrees/agent",
        ],
    );
    git(
        &project,
        &["worktree", "add", "-q", "-b", "other", "tools/wt"],
    );
}

fn assert_left_out(view: &Path) {
    assert!(view.join("src/a.txt").exists());
    assert!(view.join(".claude/settings.json").exists());
    assert!(view.join(".forkspaceexclude").exists());
    assert!(
        !view.join(".claude/worktrees").exists(),
        "agent worktrees copied"
    );
    assert!(!view.join("tools/wt").exists(), "nested worktree copied");
    assert!(view.join("tools").exists());
    assert!(!view.join("big").exists(), ".forkspaceexclude ignored");
}

#[test]
#[serial]
fn forks_leave_out_nested_worktrees_and_excluded_paths() {
    let env = Env::new();
    project(&env);
    env.ok(&["new", "copied"]);
    env.ok(&["new", "default"]);
    for name in ["copied", "default"] {
        let view = env.view(name);
        assert_left_out(&view);
        let diff = stdout(&env.ok(&["diff", name]));
        assert!(
            diff.trim().is_empty() || diff.contains("clean"),
            "{name}: {diff}"
        );
    }
    // A later fork reuses what the first one froze; the left-out paths stay out.
    env.ok(&["new", "again"]);
    assert_left_out(&env.view("again"));
}

#[test]
#[serial]
fn left_out_paths_never_move_between_a_workspace_and_its_project() {
    let env = Env::new();
    project(&env);
    env.fork_copy("w");
    let view = env.view("w");
    write(&view.join("src/b.txt"), "b\n");
    write(&view.join(".claude/worktrees/new/file.txt"), "agent\n");
    write(&view.join("big/new.bin"), "big\n");
    env.ok(&["merge", "w"]);
    let project = env.project();
    assert_eq!(
        fs::read_to_string(project.join("src/b.txt")).unwrap(),
        "b\n"
    );
    assert!(!project.join(".claude/worktrees/new").exists());
    assert!(!project.join("big/new.bin").exists());

    write(&project.join("src/c.txt"), "c\n");
    write(&project.join(".claude/worktrees/agent/late.txt"), "late\n");
    write(&project.join("big/late.bin"), "late\n");
    env.ok(&["pull", "w"]);
    assert_eq!(fs::read_to_string(view.join("src/c.txt")).unwrap(), "c\n");
    assert!(!view.join(".claude/worktrees/agent").exists());
    assert!(!view.join("big/late.bin").exists());
}

#[test]
#[serial]
fn forkspaceexclude_can_take_back_the_claude_worktrees_rule() {
    let env = Env::new();
    let project = env.project();
    write(&project.join(".claude/worktrees/notes/todo.txt"), "keep\n");
    write(&project.join(".forkspaceexclude"), "!.claude/worktrees/\n");
    env.fork_copy("w");
    assert!(env
        .view("w")
        .join(".claude/worktrees/notes/todo.txt")
        .exists());
}
