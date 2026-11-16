//! `git::head_commit` reads HEAD from `.git` without starting git, and agrees with
//! `git rev-parse` for loose refs, packed refs, a detached HEAD, and an unborn branch.

use std::path::Path;
use std::process::Command;

use forkspace::git::{
    current_branch, head_commit, ref_exists, resolve, rev_parse, switch_new_branch,
};

fn git(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

#[test]
fn head_is_read_like_git_reads_it() {
    let repo = tempfile::tempdir().unwrap();
    let path = repo.path();
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    assert!(head_commit(path).is_err(), "an unborn branch has no commit");

    std::fs::write(path.join("a"), "a").unwrap();
    git(path, &["add", "a"]);
    git(path, &["commit", "--quiet", "-m", "one"]);
    let loose = rev_parse(path, "HEAD").unwrap();
    assert_eq!(head_commit(path).unwrap(), loose);

    assert_eq!(current_branch(path).as_deref(), Some("main"));
    assert!(ref_exists(path, "refs/heads/main"));
    assert!(!ref_exists(path, "refs/heads/missing"));

    git(path, &["pack-refs", "--all"]);
    assert!(!path.join(".git/refs/heads/main").exists());
    assert_eq!(head_commit(path).unwrap(), loose);
    assert_eq!(resolve(path, "refs/heads/main").unwrap(), loose);
    assert!(ref_exists(path, "refs/heads/main"));
    assert!(!ref_exists(path, "refs/heads/missing"));

    std::fs::write(path.join("a"), "b").unwrap();
    git(path, &["commit", "--quiet", "-am", "two"]);
    git(path, &["checkout", "--quiet", "--detach", &loose]);
    assert_eq!(head_commit(path).unwrap(), loose);
    assert_eq!(current_branch(path), None);

    // A new branch written directly is one git itself agrees on.
    switch_new_branch(path, "topic/one").unwrap();
    assert_eq!(current_branch(path).as_deref(), Some("topic/one"));
    assert_eq!(rev_parse(path, "refs/heads/topic/one").unwrap(), loose);
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.status.success() && status.stdout.is_empty());
    assert!(
        switch_new_branch(path, "topic/one").is_err(),
        "an existing branch is refused"
    );
}
