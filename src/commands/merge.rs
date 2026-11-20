//! `forkspace merge`: put a workspace's changes into its project, as edits you review and
//! commit yourself, and keep the commits the workspace made as a branch of the project. The
//! workspace stays; `forkspace undo` takes a merge back.

use std::path::{Path, PathBuf};

use crate::changes::{ChangeCounts, FileChange};
use crate::cli::MergeArgs;
use crate::error::Result;
use crate::git;
use crate::state::State;
use crate::sync::journal::{self, OperationKind, OperationPhase};
use crate::sync::plan::{self, PlanOptions};
use crate::sync::{install, join_paths, merge, undo};
use crate::workspace::{guard, Workspace};

/// The outcome of the apply step: the journal still to close, what was transferred (nothing
/// when finishing an interrupted merge, whose plan is no longer known), and whether the
/// transfer can change the installed dependencies (assumed when finishing one).
struct Applied {
    journal: Option<PathBuf>,
    changes: Vec<FileChange>,
    dependencies_changed: bool,
    resumed: bool,
    /// Files the project and the workspace both changed, combined line by line.
    line_merged: Vec<PathBuf>,
}

pub fn exec(args: MergeArgs) -> Result {
    let name = crate::workspace::pick(args.name.clone())?;
    // A merge of this workspace that was interrupted is finished first.
    let resume = journal::find_open(OperationKind::Merge, &name)?.is_some();
    if !resume {
        crate::changes::manifest::settle_now(&name, true)?;
    }
    State::with_state(|state| {
        guard::ensure_no_interrupted_pull(&name)?;
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;

        let applied = apply(resume, &workspace)?;
        if applied.dependencies_changed {
            if let Some(manager) = install::detect_package_manager(&workspace) {
                install::run_install(&workspace.base, manager)?;
                println!("ran package install");
            }
        }
        let mut held = Vec::new();
        let mut undo_record = None;
        if let Some(path) = &applied.journal {
            held = journal::held(path)?;
            undo_record = journal::load(path)?.undo;
            journal::finish_undo(path)?;
            journal::set_phase(path, OperationPhase::Done)?;
            journal::remove(path)?;
        }
        state
            .require_workspace_mut(&name)?
            .unresolved_conflicts
            .clear();

        if applied.resumed {
            println!("finished the interrupted merge of '{name}'");
        }
        // What was held was not merged (MERGE-37).
        let counts = ChangeCounts::of(
            applied
                .changes
                .iter()
                .filter(|change| !held.contains(&change.path)),
        );
        if !applied.resumed {
            if applied.changes.is_empty() {
                println!("nothing to merge in '{name}'");
            } else if held.is_empty() {
                println!(
                    "merged {} files into {} ({} added, {} modified, {} deleted)",
                    counts.total(),
                    workspace.base.display(),
                    counts.added,
                    counts.modified,
                    counts.deleted
                );
            } else {
                println!(
                    "merged {} files; left {} alone that changed while merging",
                    counts.total(),
                    held.len()
                );
            }
        }
        if !applied.line_merged.is_empty() {
            println!("merged line by line: {}", join_paths(&applied.line_merged));
        }
        keep_commits(&workspace, undo_record.as_deref());
        if !held.is_empty() {
            return Err(merge::held_error(&held));
        }
        Ok(())
    })
}

/// Finish an interrupted merge, or plan and apply a new one.
fn apply(resume: bool, workspace: &Workspace) -> Result<Applied> {
    if resume {
        let path = merge::resume(workspace)?;
        return Ok(Applied {
            journal: Some(path),
            changes: Vec::new(),
            dependencies_changed: true,
            resumed: true,
            line_merged: Vec::new(),
        });
    }
    let plan = plan::build_with(
        workspace,
        PlanOptions {
            overwrite_base: false,
            preview: false,
            install_runs: true,
        },
    )?;
    let journal = if plan.changes.is_empty() {
        None
    } else {
        Some(merge::execute(&plan)?)
    };
    Ok(Applied {
        journal,
        dependencies_changed: install::touches_dependencies(
            plan.changes.iter().map(|change| &change.path),
        ),
        line_merged: plan.auto_merged.clone(),
        changes: plan.changes,
        resumed: false,
    })
}

/// Keep the commits the workspace made on its branch as a branch of the project, named after
/// the workspace (`<name>-2`, ... when the project has a branch of that name that went another
/// way, or is checked out). Never moves the project's checked-out branch or its files. A
/// failure here leaves the merged files as they are and says why.
fn keep_commits(workspace: &Workspace, undo_record: Option<&Path>) {
    match save_branch(workspace) {
        Ok(Some(saved)) => {
            println!(
                "kept the workspace's commits as branch '{}' of the project",
                saved.name
            );
            if let Some(record) = undo_record {
                if let Err(error) = undo::record_branch(record, &saved) {
                    eprintln!(
                        "warning: `undo` will not remove branch '{}': {error}",
                        saved.name
                    );
                }
            }
        }
        Ok(None) => {}
        Err(error) => eprintln!(
            "warning: could not keep the workspace's commits as a branch of the project: {error}"
        ),
    }
}

fn save_branch(workspace: &Workspace) -> Result<Option<undo::SavedBranch>> {
    let Some(branch) = &workspace.branch else {
        return Ok(None);
    };
    let (view, project) = (&workspace.view_path, &workspace.base);
    if !view.join(".git").exists() || !git::is_repo(project) {
        return Ok(None);
    }
    let Ok(tip) = git::resolve(view, &format!("refs/heads/{branch}")) else {
        return Ok(None);
    };
    if workspace.git_head_at_fork.as_deref() == Some(tip.as_str()) {
        return Ok(None);
    }
    let staging = format!("refs/forkspace/merge/{}", workspace.name);
    git::fetch_refspec(project, view, branch, &staging)?;
    let checked_out = git::current_branch(project);
    let mut number = 1;
    let saved = loop {
        let name = if number == 1 {
            branch.clone()
        } else {
            format!("{branch}-{number}")
        };
        number += 1;
        if checked_out.as_deref() == Some(name.as_str()) {
            continue;
        }
        let reference = format!("refs/heads/{name}");
        let previous = git::ref_exists(project, &reference)
            .then(|| git::resolve(project, &reference))
            .transpose()?;
        match &previous {
            Some(existing) if *existing == tip => {
                git::delete_ref(project, &staging);
                return Ok(None);
            }
            Some(existing) if !git::is_ancestor(project, existing, &tip)? => continue,
            _ => {}
        }
        git::update_ref(project, &reference, &tip)?;
        break undo::SavedBranch {
            name,
            previous,
            tip: tip.clone(),
        };
    };
    git::delete_ref(project, &staging);
    Ok(Some(saved))
}
