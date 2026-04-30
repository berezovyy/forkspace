//! `forkspace land`: publish a workspace's Git branch into its root base repository.

use std::path::{Path, PathBuf};

use crate::cli::LandArgs;
use crate::error::{ForkspaceError, Result};
use crate::git;
use crate::state::State;
use crate::workspace::{guard, Workspace};

const MERGE_INSTEAD: &str = "use `forkspace merge` for file-level transfer";

pub fn exec(args: LandArgs) -> Result {
    State::with_state(|state| {
        let workspace = state.require_workspace(&args.name)?.clone();
        guard::require_live_view(state, &workspace)?;
        let base = root_base(state, &args.name)?;
        ensure_landable(&args, &workspace, &base)?;

        let source_branch = source_branch(&workspace)?;
        let target_branch = args
            .as_branch
            .clone()
            .unwrap_or_else(|| source_branch.clone());
        ensure_valid_target(&base, &target_branch)?;

        let base_tip = git::rev_parse(&base, "HEAD")?;
        ensure_no_external_objects(&workspace.view_path, &base_tip, &source_branch)?;

        git::prune_staging_refs(&base);
        let staging = format!("refs/forkspace/land/{}", workspace.name);
        git::fetch_refspec(&base, &workspace.view_path, &source_branch, &staging)?;
        let outcome = move_target_branch(&base, &staging, &target_branch, &base_tip, args.force);
        git::delete_ref(&base, &staging);
        let message = outcome?;

        if args.drop {
            crate::workspace::remove(state, &args.name)?;
        }
        println!("{message}");
        if args.drop {
            println!("removed '{}'", args.name);
        }
        Ok(())
    })
}

/// The base of the workspace's root ancestor: the repository branches land in.
fn root_base(state: &State, name: &str) -> Result<PathBuf> {
    state
        .parent_chain(name)
        .first()
        .map(|root| root.base.clone())
        .ok_or_else(|| ForkspaceError::WorkspaceNotFound(name.to_string()))
}

/// Both sides must exist and be Git repositories, and the workspace must have no uncommitted
/// tracked changes. Untracked files only warrant a warning.
fn ensure_landable(args: &LandArgs, workspace: &Workspace, base: &Path) -> Result {
    let view = &workspace.view_path;
    for (kind, path) in [("base", base), ("view", view.as_path())] {
        if !path.is_dir() {
            return Err(ForkspaceError::RecordedPathMissing {
                workspace: args.name.clone(),
                kind: kind.to_string(),
                path: path.display().to_string(),
            });
        }
    }
    if !git::is_repo(base) {
        return Err(ForkspaceError::CapabilityUnsupported {
            diagnosis: format!("recorded base {} is not a Git repository", base.display()),
            remedy: MERGE_INSTEAD.to_string(),
        });
    }
    if !git::is_repo(view) {
        return Err(ForkspaceError::CapabilityUnsupported {
            diagnosis: "workspace view is not an independent Git repository".to_string(),
            remedy: MERGE_INSTEAD.to_string(),
        });
    }
    if !git::status(view, false)?.is_empty() {
        return Err(ForkspaceError::WorkspaceDirty(args.name.clone()));
    }
    let status = git::status(view, true)?;
    if status.lines().any(|line| line.starts_with("??")) {
        let consequence = if args.drop {
            " and will be discarded by --drop"
        } else {
            ""
        };
        eprintln!("warning: untracked files are not landed{consequence}");
    }
    Ok(())
}

/// The branch recorded at fork time, else the view's current branch.
fn source_branch(workspace: &Workspace) -> Result<String> {
    workspace
        .branch
        .clone()
        .or_else(|| git::current_branch(&workspace.view_path))
        .ok_or_else(|| ForkspaceError::CapabilityUnsupported {
            diagnosis: "workspace HEAD is detached and no branch was recorded".to_string(),
            remedy: "use `forkspace merge`, or switch to a topic branch first".to_string(),
        })
}

/// Landing never targets the default branch or the branch checked out in the base.
fn ensure_valid_target(base: &Path, target_branch: &str) -> Result {
    git::validate_branch(base, target_branch)?;
    if target_branch == git::default_branch(base) {
        return Err(ForkspaceError::CapabilityUnsupported {
            diagnosis: format!("refusing to land onto default branch '{target_branch}'"),
            remedy: "land onto a topic branch with `--as <branch>`".to_string(),
        });
    }
    if git::current_branch(base).as_deref() == Some(target_branch) {
        return Err(ForkspaceError::BranchCheckedOut {
            branch: target_branch.to_string(),
        });
    }
    Ok(())
}

/// Submodule commits and Git LFS objects live outside the fetched history, so landing them
/// would publish dangling references.
fn ensure_no_external_objects(view: &Path, base_tip: &str, source_branch: &str) -> Result {
    let external = git::external_object_changes(view, base_tip, source_branch)?;
    if external.is_empty() {
        return Ok(());
    }
    Err(ForkspaceError::CapabilityUnsupported {
        diagnosis: format!(
            "land would publish external objects: {}",
            external.join(", ")
        ),
        remedy: "publish Git LFS objects and submodule commits first, or use `forkspace merge` for file-level transfer"
            .to_string(),
    })
}

/// Point `refs/heads/<target_branch>` at the staged tip: create it, fast-forward it, or (with
/// `force`) replace a diverged branch. Returns the summary line.
fn move_target_branch(
    base: &Path,
    staging: &str,
    target_branch: &str,
    base_tip: &str,
    force: bool,
) -> Result<String> {
    let tip = git::rev_parse(base, staging)?;
    let target = format!("refs/heads/{target_branch}");
    if !git::ref_exists(base, &target) {
        let count = git::commit_count(base, &format!("{base_tip}..{tip}"))?;
        git::update_ref(base, &target, &tip)?;
        return Ok(format!("created '{target_branch}' ({count} commits)"));
    }

    let old = git::rev_parse(base, &target)?;
    let count = git::commit_count(base, &format!("{old}..{tip}"))?;
    if git::is_ancestor(base, &old, &tip)? {
        git::update_ref(base, &target, &tip)?;
        return Ok(format!(
            "updated '{target_branch}' {}..{} ({count} new commits)",
            abbreviate(&old),
            abbreviate(&tip)
        ));
    }
    let discarded = git::log_oneline(base, &format!("{tip}..{old}"))?;
    if !force {
        return Err(ForkspaceError::BranchExists(format!(
            "'{target_branch}' diverged; base-side commits would be discarded:\n{discarded}\nrerun with --force to replace it"
        )));
    }
    eprintln!("warning: replacing divergent branch '{target_branch}' with workspace tip");
    git::update_ref(base, &target, &tip)?;
    Ok(format!("replaced '{target_branch}' with workspace history"))
}

fn abbreviate(revision: &str) -> &str {
    &revision[..revision.len().min(12)]
}
