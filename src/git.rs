//! Thin wrappers over the `git` CLI.
//!
//! Every invocation runs non-interactively (`GIT_TERMINAL_PROMPT=0`, null stdin). A failure to
//! spawn git, or a non-zero exit where success was required, becomes [`ForkspaceError::GitFailed`].

use std::path::{Path, PathBuf};
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
    plain_git_dir(directory).is_some() || succeeds(directory, &["rev-parse", "--git-dir"])
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
    if let Some(head) = plain_git_dir(directory).and_then(|git| read_head_file(&git)) {
        return match head {
            Head::Branch(reference) => reference
                .strip_prefix("refs/heads/")
                .filter(|branch| !branch.is_empty())
                .map(str::to_string),
            Head::Detached(_) => None,
        };
    }
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

/// The commit `HEAD` names in the repository at `directory`. A plain repository's loose or
/// packed refs are read directly, which saves starting `git`; anything else (a worktree or
/// submodule `.git` file, reftable, a symbolic ref) goes through `git rev-parse`.
pub fn head_commit(directory: &Path) -> Result<String> {
    read_head(directory).map_or_else(|| rev_parse(directory, "HEAD"), Ok)
}

fn read_head(directory: &Path) -> Option<String> {
    let git = plain_git_dir(directory)?;
    match read_head_file(&git)? {
        Head::Detached(id) => Some(id),
        Head::Branch(reference) => resolve_ref(&git, &reference)?,
    }
}

/// The `.git` directory of a plain repository at `directory` whose refs are files, or
/// `None` for anything `git` itself must be asked about (no `.git` directory, a worktree or
/// submodule `.git` file, reftable).
fn plain_git_dir(directory: &Path) -> Option<PathBuf> {
    let git = directory.join(".git");
    (git.join("HEAD").is_file() && !git.join("reftable").exists()).then_some(git)
}

/// What `.git/HEAD` says.
enum Head {
    /// `ref: refs/heads/<name>` (or another `refs/` name).
    Branch(String),
    Detached(String),
}

fn read_head_file(git: &Path) -> Option<Head> {
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) if is_safe_ref(reference) => Some(Head::Branch(reference.to_string())),
        Some(_) => None,
        None => is_object_id(head).then(|| Head::Detached(head.to_string())),
    }
}

/// The object a ref names, from its loose file or `packed-refs`: `Some(None)` when the ref
/// does not exist, `None` when that cannot be told without `git` (a symbolic ref).
fn resolve_ref(git: &Path, reference: &str) -> Option<Option<String>> {
    if !is_safe_ref(reference) {
        return None;
    }
    match std::fs::read_to_string(git.join(reference)) {
        Ok(loose) => {
            let id = loose.trim();
            return is_object_id(id).then(|| Some(id.to_string()));
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return None,
        Err(_) => {}
    }
    let packed = match std::fs::read_to_string(git.join("packed-refs")) {
        Ok(packed) => packed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(None),
        Err(_) => return None,
    };
    Some(packed.lines().find_map(|line| {
        let (id, name) = line.split_once(' ')?;
        (name == reference && is_object_id(id)).then(|| id.to_string())
    }))
}

fn is_safe_ref(reference: &str) -> bool {
    reference.starts_with("refs/")
        && !reference
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

/// A full SHA-1 or SHA-256 object id.
fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn ref_exists(directory: &Path, reference: &str) -> bool {
    if let Some(found) = plain_git_dir(directory).and_then(|git| resolve_ref(&git, reference)) {
        return found.is_some();
    }
    succeeds(directory, &["show-ref", "--verify", "--quiet", reference])
}

/// Create `branch` at the current commit and check it out, like `git switch -c` (which leaves
/// the index and files alone, the commit being the same). A plain repository gets the ref and
/// `HEAD` written directly, which saves starting `git`, though without a reflog entry for the
/// new branch; anything else, and an unborn `HEAD`, goes through `git switch -c`.
pub fn switch_new_branch(directory: &Path, branch: &str) -> Result {
    let reference = format!("refs/heads/{branch}");
    if let (Some(git), Some(commit)) = (plain_git_dir(directory), read_head(directory)) {
        if resolve_ref(&git, &reference) == Some(None) {
            return write_new_branch(&git, &reference, &commit);
        }
    }
    run_git(directory, &["switch", "-c", branch])
}

fn write_new_branch(git: &Path, reference: &str, commit: &str) -> Result {
    use std::io::Write as _;

    use crate::util::IoContext;

    let path = git.join(reference);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).io_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| file.write_all(format!("{commit}\n").as_bytes()))
        .io_context(|| format!("create {}", path.display()))?;
    // `HEAD.lock` is git's own lock name for HEAD: a concurrent git command waits or fails
    // instead of writing over it.
    let lock = git.join("HEAD.lock");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .and_then(|mut file| file.write_all(format!("ref: {reference}\n").as_bytes()))
        .and_then(|()| std::fs::rename(&lock, git.join("HEAD")))
        .io_context(|| format!("switch {} to {reference}", git.display()))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&lock);
            let _ = std::fs::remove_file(&path);
        })
}

/// The object `reference` (a full `refs/...` name) points at, read from the ref files when
/// the repository allows, else through `git rev-parse`.
pub fn resolve(directory: &Path, reference: &str) -> Result<String> {
    match plain_git_dir(directory).and_then(|git| resolve_ref(&git, reference)) {
        Some(Some(id)) => Ok(id),
        _ => rev_parse(directory, reference),
    }
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
    // Nothing to prune is the common case; seeing that needs no `git`.
    if let Some(git) = plain_git_dir(directory) {
        let loose = git.join("refs/forkspace/land");
        let packed = std::fs::read_to_string(git.join("packed-refs")).unwrap_or_default();
        if !loose.exists() && !packed.contains(" refs/forkspace/land/") {
            return;
        }
    }
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
