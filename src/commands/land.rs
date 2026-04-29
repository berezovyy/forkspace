use crate::cli::LandArgs;
use crate::error::{ForkspaceError, Result};
use crate::git;
use crate::state::State;

pub fn exec(args: LandArgs) -> Result {
    State::with_state(|state| {
        let workspace = state.require_workspace(&args.name)?.clone();
        let root = state
            .parent_chain(&args.name)
            .first()
            .copied()
            .ok_or_else(|| ForkspaceError::WorkspaceNotFound(args.name.clone()))?;
        let base = root.base.clone();
        if !base.is_dir() {
            return Err(ForkspaceError::RecordedPathMissing {
                workspace: args.name.clone(),
                kind: "base".to_string(),
                path: base.display().to_string(),
            });
        }
        if !workspace.view_path.is_dir() {
            return Err(ForkspaceError::RecordedPathMissing {
                workspace: args.name.clone(),
                kind: "view".to_string(),
                path: workspace.view_path.display().to_string(),
            });
        }
        if !git::is_repo(&base) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: format!("recorded base {} is not a Git repository", base.display()),
                remedy: "use `forkspace merge` for file-level transfer".to_string(),
            });
        }
        if !git::is_repo(&workspace.view_path) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "workspace view is not an independent Git repository".to_string(),
                remedy: "use `forkspace merge` for file-level transfer".to_string(),
            });
        }
        if !git::status(&workspace.view_path, false)?.is_empty() {
            return Err(ForkspaceError::WorkspaceDirty(args.name.clone()));
        }
        let untracked = git::status(&workspace.view_path, true)?;
        if untracked.lines().any(|line| line.starts_with("??")) {
            eprintln!(
                "warning: untracked files are not landed{}",
                if args.drop {
                    " and will be discarded by --drop"
                } else {
                    ""
                }
            );
        }

        let source_branch = workspace
            .branch
            .clone()
            .or_else(|| git::current_branch(&workspace.view_path))
            .ok_or_else(|| ForkspaceError::CapabilityUnsupported {
                diagnosis: "workspace HEAD is detached and no branch was recorded".to_string(),
                remedy: "use `forkspace merge`, or switch to a topic branch first".to_string(),
            })?;
        let target_branch = args
            .as_branch
            .clone()
            .unwrap_or_else(|| source_branch.clone());
        git::validate_branch(&base, &target_branch)?;
        if target_branch == git::default_branch(&base) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: format!("refusing to land onto default branch '{target_branch}'"),
                remedy: "land onto a topic branch with `--as <branch>`".to_string(),
            });
        }
        if git::current_branch(&base).as_deref() == Some(&target_branch) {
            return Err(ForkspaceError::BranchCheckedOut {
                branch: target_branch,
            });
        }

        let base_tip = git::rev_parse(&base, "HEAD")?;
        let external =
            git::external_object_changes(&workspace.view_path, &base_tip, &source_branch)?;
        if !external.is_empty() {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: format!(
                    "land would publish external objects: {}",
                    external.join(", ")
                ),
                remedy: "publish Git LFS objects and submodule commits first, or use `forkspace merge` for file-level transfer"
                    .to_string(),
            });
        }

        git::prune_staging_refs(&base);
        let staging = format!("refs/forkspace/land/{}", workspace.name);
        git::fetch_refspec(&base, &workspace.view_path, &source_branch, &staging)?;
        let outcome = (|| {
            let tip = git::rev_parse(&base, &staging)?;
            let target = format!("refs/heads/{target_branch}");
            if !git::ref_exists(&base, &target) {
                let count = git::commit_count(&base, &format!("{base_tip}..{tip}"))?;
                git::update_ref(&base, &target, &tip)?;
                return Ok(format!("created '{target_branch}' ({count} commits)"));
            }

            let old = git::rev_parse(&base, &target)?;
            let count = git::commit_count(&base, &format!("{old}..{tip}"))?;
            if git::is_ancestor(&base, &old, &tip)? {
                git::update_ref(&base, &target, &tip)?;
                return Ok(format!(
                    "updated '{target_branch}' {}..{} ({count} new commits)",
                    &old[..old.len().min(12)],
                    &tip[..tip.len().min(12)]
                ));
            }
            let discarded = git::log_oneline(&base, &format!("{tip}..{old}"))?;
            if !args.force {
                return Err(ForkspaceError::BranchExists(format!(
                    "'{target_branch}' diverged; base-side commits would be discarded:\n{discarded}\nrerun with --force to replace it"
                )));
            }
            eprintln!("warning: replacing divergent branch '{target_branch}' with workspace tip");
            git::update_ref(&base, &target, &tip)?;
            Ok(format!("replaced '{target_branch}' with workspace history"))
        })();
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
