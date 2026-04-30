//! Thin wrappers over the `git` CLI.
//!
//! Every invocation runs non-interactively (`GIT_TERMINAL_PROMPT=0`, null stdin). A failure to
//! spawn git, or a non-zero exit where success was required, becomes [`ForkspaceError::GitFailed`].

use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::error::{ForkspaceError, Result};

/// Run `git -C <directory> <args>` and return its raw output, whatever the exit status.
pub(crate) fn capture_output(directory: &Path, args: &[&str]) -> std::io::Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
}

/// Run git, requiring success, and return its trimmed stdout. A failure carries git's stderr.
pub(crate) fn capture_git(directory: &Path, args: &[&str]) -> Result<String> {
    let output = spawn(directory, args, || {
        format!("failed to run git {}", args.join(" "))
    })?;
    if !output.status.success() {
        let stderr = trimmed(&output.stderr);
        return Err(ForkspaceError::GitFailed(if stderr.is_empty() {
            format!("git {} exited with {}", args.join(" "), output.status)
        } else {
            stderr
        }));
    }
    Ok(trimmed(&output.stdout))
}

/// Run git, requiring success, and discard its output.
pub fn run_git(directory: &Path, args: &[&str]) -> Result {
    capture_git(directory, args).map(drop)
}

/// Whether `directory` is inside a Git work tree or repository.
pub fn is_repo(directory: &Path) -> bool {
    succeeds(directory, &["rev-parse", "--git-dir"])
}

/// Whether `.git` is a file (a linked worktree or submodule) rather than a directory.
pub fn git_dir_is_file(directory: &Path) -> bool {
    directory.join(".git").is_file()
}

/// Reject names `git check-ref-format --branch` does not accept.
pub fn validate_branch(directory: &Path, branch: &str) -> Result {
    let output = spawn(directory, &["check-ref-format", "--branch", branch], || {
        format!("failed to validate branch '{branch}'")
    })?;
    if !output.status.success() {
        return Err(ForkspaceError::InvalidArgument(format!(
            "invalid Git branch name '{branch}'; choose a name accepted by `git check-ref-format --branch`"
        )));
    }
    Ok(())
}

/// The checked-out branch's short name, or `None` when `HEAD` is detached.
pub fn current_branch(directory: &Path) -> Option<String> {
    capture_output(directory, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| trimmed(&output.stdout))
        .filter(|branch| !branch.is_empty())
}

/// The full object name `revision` resolves to.
pub fn rev_parse(directory: &Path, revision: &str) -> Result<String> {
    capture_git(directory, &["rev-parse", "--verify", revision])
}

pub fn ref_exists(directory: &Path, reference: &str) -> bool {
    succeeds(directory, &["show-ref", "--verify", "--quiet", reference])
}

/// Whether commit `old` is an ancestor of (or equal to) `new`.
pub fn is_ancestor(directory: &Path, old: &str, new: &str) -> Result<bool> {
    let output = spawn(
        directory,
        &["merge-base", "--is-ancestor", old, new],
        || "failed to compare commits".to_string(),
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(ForkspaceError::GitFailed(trimmed(&output.stderr))),
    }
}

/// `git status --porcelain`, with or without untracked files.
pub fn status(directory: &Path, include_untracked: bool) -> Result<String> {
    let untracked = if include_untracked {
        "--untracked-files=normal"
    } else {
        "--untracked-files=no"
    };
    capture_git(directory, &["status", "--porcelain", untracked])
}

/// Fetch `source_branch` from the repository at `view` into the `staging` ref of `base`,
/// overwriting any previous value.
pub fn fetch_refspec(base: &Path, view: &Path, source_branch: &str, staging: &str) -> Result {
    let refspec = format!("+refs/heads/{source_branch}:{staging}");
    let view = view.to_str().ok_or_else(|| {
        ForkspaceError::InvalidArgument(
            "workspace path is not valid UTF-8 for git fetch".to_string(),
        )
    })?;
    run_git(base, &["fetch", "--no-tags", "--quiet", view, &refspec])
}

pub fn update_ref(directory: &Path, reference: &str, tip: &str) -> Result {
    run_git(directory, &["update-ref", reference, tip])
}

/// Best-effort ref deletion; cleanup must not mask the error that triggered it.
pub fn delete_ref(directory: &Path, reference: &str) {
    let _ = capture_output(directory, &["update-ref", "-d", reference]);
}

/// Best-effort removal of staging refs left behind by interrupted `land` runs.
pub fn prune_staging_refs(directory: &Path) {
    let Ok(refs) = capture_git(
        directory,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/forkspace/land/",
        ],
    ) else {
        return;
    };
    for reference in refs.lines().filter(|line| !line.is_empty()) {
        delete_ref(directory, reference);
    }
}

/// Number of commits in `range` (e.g. `a..b`).
pub fn commit_count(directory: &Path, range: &str) -> Result<usize> {
    capture_git(directory, &["rev-list", "--count", range])?
        .parse()
        .map_err(|_| ForkspaceError::GitFailed(format!("invalid commit count for {range}")))
}

pub fn log_oneline(directory: &Path, range: &str) -> Result<String> {
    capture_git(directory, &["log", "--oneline", range])
}

/// Paths added or modified in `base_tip..source_branch` whose content lives outside the
/// repository: submodule commits and Git LFS pointers. Each entry is a display label.
pub fn external_object_changes(
    directory: &Path,
    base_tip: &str,
    source_branch: &str,
) -> Result<Vec<String>> {
    let range = format!("{base_tip}..{source_branch}");
    let raw = capture_git(directory, &["diff", "--raw", "--no-renames", &range])?;
    let mut external = Vec::new();
    for line in raw.lines() {
        // `:<old mode> <new mode> <old sha> <new sha> <status>\t<path>`
        let Some((header, path)) = line.split_once('\t') else {
            continue;
        };
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields.len() < 5 || fields[4].starts_with('D') {
            continue;
        }
        if fields[1] == SUBMODULE_MODE {
            external.push(format!("submodule {path}"));
        } else if is_lfs_pointer(directory, source_branch, path)? {
            external.push(format!("Git LFS pointer {path}"));
        }
    }
    Ok(external)
}

/// The repository's default branch: `origin/HEAD`, else `init.defaultBranch`, else `main` if it
/// exists, else `master`.
pub fn default_branch(directory: &Path) -> String {
    let origin_head = capture_git(
        directory,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    );
    if let Some((_, branch)) = origin_head
        .as_deref()
        .ok()
        .and_then(|head| head.split_once('/'))
    {
        return branch.to_string();
    }
    if let Ok(configured) = capture_git(directory, &["config", "--get", "init.defaultBranch"]) {
        if !configured.is_empty() {
            return configured;
        }
    }
    if ref_exists(directory, "refs/heads/main") {
        "main".to_string()
    } else {
        "master".to_string()
    }
}

const SUBMODULE_MODE: &str = "160000";
const LFS_POINTER_HEADER: &[u8] = b"version https://git-lfs.github.com/spec/v1\n";

fn is_lfs_pointer(directory: &Path, revision: &str, path: &str) -> Result<bool> {
    let object = format!("{revision}:{path}");
    let output = spawn(directory, &["show", &object], || {
        format!("failed to inspect landed object {path}")
    })?;
    Ok(output.status.success() && output.stdout.starts_with(LFS_POINTER_HEADER))
}

/// Run git, mapping only a failure to spawn it into `GitFailed("<context>: <error>")`.
fn spawn(directory: &Path, args: &[&str], context: impl FnOnce() -> String) -> Result<Output> {
    capture_output(directory, args)
        .map_err(|error| ForkspaceError::GitFailed(format!("{}: {error}", context())))
}

fn succeeds(directory: &Path, args: &[&str]) -> bool {
    capture_output(directory, args).is_ok_and(|output| output.status.success())
}

fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}
