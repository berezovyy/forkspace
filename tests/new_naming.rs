//! `new` copies the project you are in, names the workspace `<project>-N`, and gives a Git
//! project's workspace a branch of that name (`src/commands/new.rs`).

mod common;

use std::path::Path;
use std::process::Command;

use common::cli::{stderr, write, Env};
use serial_test::serial;

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
#[serial]
fn workspaces_are_numbered_and_get_a_branch_of_their_name() {
    let env = Env::new();
    let project = env.project();
    write(&project.join("a.txt"), "a\n");
    git(&project, &["init", "--quiet", "--initial-branch=main"]);
    git(&project, &["config", "user.email", "t@example.invalid"]);
    git(&project, &["config", "user.name", "T"]);
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "--quiet", "-m", "one"]);
    // A branch the project already has is skipped, not reused.
    git(&project, &["branch", "project-2"]);

    env.ok(&["new"]);
    env.ok(&["new"]);
    let names: Vec<String> = env.workspaces();
    assert_eq!(names, ["project-1", "project-3"]);
    for name in &names {
        assert_eq!(git(&env.view(name), &["branch", "--show-current"]), *name);
    }
    assert_eq!(git(&project, &["branch", "--show-current"]), "main");

    let taken = env.run(&["new", "project-2"]);
    assert!(!taken.status.success());
    assert!(
        stderr(&taken).contains("already has a branch 'project-2'"),
        "{}",
        stderr(&taken)
    );
}

/// A project folder inside another repository without a `.git` of its own (here: a folder
/// of a repository's ignored build directory) gets no branch, and the outer repository is
/// left on its branch.
#[test]
#[serial]
fn a_folder_inside_another_repository_never_switches_that_repository() {
    let env = Env::new();
    let outer = env.outside().join("outer");
    std::fs::create_dir_all(&outer).unwrap();
    git(&outer, &["init", "--quiet", "--initial-branch=main"]);
    git(&outer, &["config", "user.email", "t@example.invalid"]);
    git(&outer, &["config", "user.name", "T"]);
    write(&outer.join(".gitignore"), "build/\n");
    git(&outer, &["add", "-A"]);
    git(&outer, &["commit", "--quiet", "-m", "outer"]);
    let inner = outer.join("build/app");
    write(&inner.join("a.txt"), "a\n");

    let output = common::new_in(env.home.path(), &inner, &["w"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(git(&outer, &["branch", "--show-current"]), "main");
    assert_eq!(git(&outer, &["branch", "--list", "w"]), "");
}
