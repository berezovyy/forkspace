//! `forkspace hook`: Claude Code's `WorktreeCreate` and `WorktreeRemove` hooks.
//!
//! With these hooks configured, the worktrees Claude Code makes for `claude --worktree`,
//! subagents with `isolation: worktree` and background sessions are forkspace workspaces:
//! `node_modules`, `.env` and the rest of the project come along, which a Git worktree (and
//! `.worktreeinclude`, which Claude Code skips when a hook is set) would not provide.
//!
//! The contract (code.claude.com/docs/en/hooks#worktreecreate): the hook reads JSON on stdin;
//! `WorktreeCreate` gets `name` and `cwd` and prints the worktree's absolute path as the last
//! line of stdout, anything else going to stderr; a non-zero exit fails the creation.
//! `WorktreeRemove` gets `worktree_path`; exit 0 means the directory is gone, a non-zero exit
//! with the directory still there keeps it.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli::{HookArgs, HookEvent};
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::workspace::{self, CreateOptions, Workspace};

pub fn exec(args: HookArgs) -> Result {
    match args.event {
        HookEvent::WorktreeCreate => worktree_create(&read_input()?),
        HookEvent::WorktreeRemove { discard } => worktree_remove(&read_input()?, discard),
        HookEvent::ClaudeSettings => {
            println!("{}", serde_json::to_string_pretty(&claude_settings())?);
            Ok(())
        }
    }
}

/// The `hooks` section to merge into Claude Code's `settings.json`.
pub fn claude_settings() -> Value {
    serde_json::json!({
        "hooks": {
            "WorktreeCreate": [
                {"hooks": [{"type": "command", "command": "forkspace hook worktree-create"}]}
            ],
            "WorktreeRemove": [
                {"hooks": [{"type": "command", "command": "forkspace hook worktree-remove"}]}
            ]
        }
    })
}

fn read_input() -> Result<Value> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| ForkspaceError::InvalidArgument(format!("read hook input: {error}")))?;
    serde_json::from_str(&input).map_err(|error| {
        ForkspaceError::InvalidArgument(format!("hook input is not JSON: {error}"))
    })
}

/// Fork the project Claude Code runs in into a workspace named after the worktree, or reuse
/// the workspace of that name forked from the same project, and print its path.
fn worktree_create(input: &Value) -> Result {
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .map(workspace_name)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            ForkspaceError::InvalidArgument("WorktreeCreate input has no \"name\"".to_string())
        })?;
    let project = project_dir(input)?;

    let existing = State::load()?.workspaces.get(&name).cloned();
    let workspace = match existing {
        Some(workspace) if same_dir(&workspace.base, &project) => {
            eprintln!("forkspace: reusing workspace '{name}'");
            workspace
        }
        Some(workspace) => {
            return Err(ForkspaceError::InvalidArgument(format!(
                "workspace '{name}' was forked from {}, not {}; pick another worktree name",
                workspace.base.display(),
                project.display()
            )))
        }
        None => create(&project, &name, input)?,
    };
    // Claude Code reads the last line of stdout; everything else went to stderr.
    println!("{}", workspace.view_path.display());
    Ok(())
}

fn create(project: &Path, name: &str, input: &Value) -> Result<Workspace> {
    let session = input
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let options = |branch: Option<String>| CreateOptions {
        session: session.clone(),
        branch,
        ..CreateOptions::default()
    };
    // Like Claude Code's own worktrees: a branch named `worktree-<name>`, when the project is
    // a Git repository and the branch is free.
    let branch = project
        .join(".git")
        .exists()
        .then(|| format!("worktree-{name}"));
    match workspace::create(project.to_path_buf(), name, options(branch.clone())) {
        Err(ForkspaceError::BranchExists(_)) if branch.is_some() => {
            eprintln!(
                "forkspace: branch '{}' already exists; the workspace keeps the project's current branch",
                branch.unwrap_or_default()
            );
            workspace::create(project.to_path_buf(), name, options(None))
        }
        created => created,
    }
}

/// Drop the workspace at `worktree_path`. Without `discard`, a workspace with changes the
/// project does not have (files, or commits on its branch) is kept, and the hook fails, so
/// Claude Code keeps it too; `forkspace merge`, `land` or `drop` then decide.
fn worktree_remove(input: &Value, discard: bool) -> Result {
    let path = input
        .get("worktree_path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| {
            ForkspaceError::InvalidArgument(
                "WorktreeRemove input has no \"worktree_path\"".to_string(),
            )
        })?;
    let state = State::load()?;
    let Some(workspace) = state
        .workspaces
        .values()
        .find(|workspace| same_dir(&workspace.view_path, &path))
        .cloned()
    else {
        return Err(ForkspaceError::InvalidArgument(format!(
            "no forkspace workspace is at {}",
            path.display()
        )));
    };
    if !discard {
        let unmerged = unmerged_work(&workspace)?;
        if !unmerged.is_empty() {
            return Err(ForkspaceError::InvalidArgument(format!(
                "kept workspace '{}': {}; review it with 'forkspace diff {0}', then merge, land or drop it",
                workspace.name,
                unmerged.join(", ")
            )));
        }
    }
    State::with_state(|state| workspace::remove::remove(state, &workspace.name))?;
    eprintln!("forkspace: dropped workspace '{}'", workspace.name);
    Ok(())
}

/// What `workspace` holds that its project does not: changed files a merge would bring over,
/// and commits made since the fork.
fn unmerged_work(workspace: &Workspace) -> Result<Vec<String>> {
    let backend = crate::backend::select_backend(Some(workspace.backend_kind))?;
    let changes = crate::changes::filter::ChangeFilter::build(&workspace.base)
        .filter_changes(backend.transferable_changes(workspace)?);
    let mut found = Vec::new();
    if !changes.is_empty() {
        found.push(format!("{} changed file(s)", changes.len()));
    }
    let head = crate::git::head_commit(&workspace.view_path).ok();
    if head.is_some() && head != workspace.git_head_at_fork {
        found.push("new commits".to_string());
    }
    Ok(found)
}

/// The project Claude Code runs in: `CLAUDE_PROJECT_DIR`, else the hook input's `cwd`, else
/// this process's directory, widened to its Git repository's top level.
fn project_dir(input: &Value) -> Result<PathBuf> {
    let dir = std::env::var_os("CLAUDE_PROJECT_DIR")
        .map(PathBuf::from)
        .or_else(|| input.get("cwd").and_then(Value::as_str).map(PathBuf::from))
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|error| ForkspaceError::InvalidArgument(format!("project directory: {error}")))?;
    let top = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()));
    let dir = top.unwrap_or(dir);
    dir.canonicalize()
        .map_err(|error| ForkspaceError::InvalidArgument(format!("{}: {error}", dir.display())))
}

/// A workspace name for the worktree name Claude Code chose: its letters, digits, `.`, `_`
/// and `-`, other characters becoming `-`.
fn workspace_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    cleaned.trim_start_matches('.').to_string()
}

fn same_dir(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_names_become_workspace_names() {
        assert_eq!(workspace_name("bold-oak-a3f2"), "bold-oak-a3f2");
        assert_eq!(workspace_name("feature/auth x"), "feature-auth-x");
        assert_eq!(workspace_name(".hidden"), "hidden");
    }
}
