//! `git::head_commit` reads HEAD from `.git` without starting git, and agrees with
//! `git rev-parse` for loose refs, packed refs, a detached HEAD, and an unborn branch.

use std::path::Path;
use std::process::Command;

use forkspace::git::{head_commit, rev_parse};

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

    git(path, &["pack-refs", "--all"]);
    assert!(!path.join(".git/refs/heads/main").exists());
    assert_eq!(head_commit(path).unwrap(), loose);

    std::fs::write(path.join("a"), "b").unwrap();
    git(path, &["commit", "--quiet", "-am", "two"]);
    git(path, &["checkout", "--quiet", "--detach", &loose]);
    assert_eq!(head_commit(path).unwrap(), loose);
}
