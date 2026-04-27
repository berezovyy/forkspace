use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::error::{ForkspaceError, Result};

pub fn capture_output(directory: &Path, args: &[&str]) -> std::io::Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
}

pub fn capture_git(directory: &Path, args: &[&str]) -> Result<String> {
    let output = capture_output(directory, args).map_err(|error| {
        ForkspaceError::GitFailed(format!("failed to run git {}: {error}", args.join(" ")))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ForkspaceError::GitFailed(if stderr.is_empty() {
            format!("git {} exited with {}", args.join(" "), output.status)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn run_git(directory: &Path, args: &[&str]) -> Result {
    capture_git(directory, args).map(|_| ())
}

pub fn is_repo(directory: &Path) -> bool {
    capture_output(directory, &["rev-parse", "--git-dir"])
        .is_ok_and(|output| output.status.success())
}

pub fn git_dir_is_file(directory: &Path) -> bool {
    directory.join(".git").is_file()
}

pub fn validate_branch(directory: &Path, branch: &str) -> Result {
    let output =
        capture_output(directory, &["check-ref-format", "--branch", branch]).map_err(|error| {
            ForkspaceError::GitFailed(format!("failed to validate branch '{branch}': {error}"))
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(ForkspaceError::InvalidArgument(format!(
            "invalid Git branch name '{branch}'; choose a name accepted by `git check-ref-format --branch`"
        )))
    }
}

pub fn current_branch(directory: &Path) -> Option<String> {
    capture_output(directory, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn rev_parse(directory: &Path, revision: &str) -> Result<String> {
    capture_git(directory, &["rev-parse", "--verify", revision])
}

pub fn ref_exists(directory: &Path, reference: &str) -> bool {
    capture_output(directory, &["show-ref", "--verify", "--quiet", reference])
        .is_ok_and(|output| output.status.success())
}

pub fn is_ancestor(directory: &Path, old: &str, new: &str) -> Result<bool> {
    let output =
        capture_output(directory, &["merge-base", "--is-ancestor", old, new]).map_err(|error| {
            ForkspaceError::GitFailed(format!("failed to compare commits: {error}"))
        })?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(ForkspaceError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )),
    }
}

pub fn status(directory: &Path, include_untracked: bool) -> Result<String> {
    capture_git(
        directory,
        &[
            "status",
            "--porcelain",
            if include_untracked {
                "--untracked-files=normal"
            } else {
                "--untracked-files=no"
            },
        ],
    )
}

pub fn fetch_refspec(base: &Path, view: &Path, source_branch: &str, staging: &str) -> Result {
    let source = format!("refs/heads/{source_branch}");
    let refspec = format!("+{source}:{staging}");
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

pub fn delete_ref(directory: &Path, reference: &str) {
    let _ = capture_output(directory, &["update-ref", "-d", reference]);
}

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

pub fn commit_count(directory: &Path, range: &str) -> Result<usize> {
    capture_git(directory, &["rev-list", "--count", range])?
        .parse()
        .map_err(|_| ForkspaceError::GitFailed(format!("invalid commit count for {range}")))
}

pub fn log_oneline(directory: &Path, range: &str) -> Result<String> {
    capture_git(directory, &["log", "--oneline", range])
}

pub fn external_object_changes(
    directory: &Path,
    base_tip: &str,
    source_branch: &str,
) -> Result<Vec<String>> {
    let range = format!("{base_tip}..{source_branch}");
    let raw = capture_git(directory, &["diff", "--raw", "--no-renames", &range])?;
    let mut external = Vec::new();
    for line in raw.lines() {
        let Some((header, path)) = line.split_once('\t') else {
            continue;
        };
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields.len() < 5 || fields[4].starts_with('D') {
            continue;
        }
        if fields[1] == "160000" {
            external.push(format!("submodule {path}"));
            continue;
        }
        let object = format!("{source_branch}:{path}");
        let output = capture_output(directory, &["show", &object]).map_err(|error| {
            ForkspaceError::GitFailed(format!("failed to inspect landed object {path}: {error}"))
        })?;
        if output.status.success()
            && output
                .stdout
                .starts_with(b"version https://git-lfs.github.com/spec/v1\n")
        {
            external.push(format!("Git LFS pointer {path}"));
        }
    }
    Ok(external)
}

pub fn default_branch(directory: &Path) -> String {
    if let Ok(value) = capture_git(
        directory,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        if let Some((_, branch)) = value.split_once('/') {
            return branch.to_string();
        }
    }
    if let Ok(value) = capture_git(directory, &["config", "--get", "init.defaultBranch"]) {
        if !value.is_empty() {
            return value;
        }
    }
    if ref_exists(directory, "refs/heads/main") {
        "main".to_string()
    } else {
        "master".to_string()
    }
}
