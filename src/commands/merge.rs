//! `forkspace merge`: apply a workspace's changes to its base, or print them as a patch.

use std::path::PathBuf;

use crate::changes::ChangeCounts;
use crate::cli::MergeArgs;
use crate::error::Result;
use crate::review::signals;
use crate::state::State;
use crate::sync::journal::{self, OperationPhase};
use crate::sync::plan::{self, MergePlan, PlanOptions};
use crate::sync::{install, join_paths, merge, undo};
use crate::workspace::{guard, Workspace};

/// The outcome of the apply step: the journal still to close, and what was transferred
/// (zero when resuming, where the original plan is no longer known).
struct Applied {
    journal: Option<PathBuf>,
    counts: ChangeCounts,
}

pub fn exec(args: MergeArgs) -> Result {
    if args.undo {
        return undo(&args);
    }
    let name = args.name.clone().unwrap_or_default();
    State::with_state(|state| {
        guard::ensure_no_interrupted_pull(&name)?;
        if !args.resume {
            guard::ensure_no_interrupted_merge(&name)?;
        }
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;

        if args.patch {
            if print_patch(&name, &workspace)? && args.drop {
                crate::workspace::remove(state, &name)?;
                eprintln!("removed '{}'", name);
            }
            return Ok(());
        }

        let Some(applied) = apply(&args, &workspace)? else {
            return Ok(());
        };
        run_post_merge_steps(&args, &workspace)?;
        if let Some(path) = &applied.journal {
            journal::finish_undo(path)?;
            journal::set_phase(path, OperationPhase::Done)?;
            journal::remove(path)?;
        }

        state
            .require_workspace_mut(&name)?
            .unresolved_conflicts
            .clear();
        let counts = applied.counts;
        if counts.total() == 0 && !args.resume {
            println!("nothing to merge in '{}'", name);
            return Ok(());
        }
        if args.drop {
            crate::workspace::remove(state, &name)?;
        }
        println!(
            "merged {} files ({} added, {} modified, {} deleted)",
            counts.total(),
            counts.added,
            counts.modified,
            counts.deleted
        );
        if args.drop {
            println!("removed '{}'", name);
        }
        Ok(())
    })
}

/// Print the workspace's changes as a unified patch. Returns whether there was anything to print.
fn print_patch(name: &str, workspace: &Workspace) -> Result<bool> {
    let plan = plan::build(workspace, true)?;
    if plan.changes.is_empty() {
        println!("nothing to merge in '{name}'");
        return Ok(false);
    }
    print!("{}", merge::generate_patch(workspace, &plan.changes)?);
    Ok(true)
}

/// Resume an interrupted merge, or plan, review, and apply a new one.
/// Returns `None` for a dry run, which stops after printing the plan.
fn apply(args: &MergeArgs, workspace: &Workspace) -> Result<Option<Applied>> {
    if args.resume {
        return Ok(Some(Applied {
            journal: Some(merge::resume(workspace)?),
            counts: ChangeCounts::default(),
        }));
    }

    let overwrite = args.overwrite_base || args.force;
    let plan = plan::build_with(
        workspace,
        PlanOptions {
            overwrite_base: overwrite,
            preview: args.dry_run,
            install_runs: !args.no_install,
        },
    )?;
    let review = review_plan(workspace, &plan);
    if args.dry_run {
        println!("{review}");
        return Ok(None);
    }
    if overwrite && !plan.conflicts.is_empty() {
        eprintln!(
            "warning: overwriting base changes: {}",
            join_paths(&plan.conflicts)
        );
    }
    if merge::workspace_head_changed(workspace)? {
        eprintln!(
            "warning: workspace git HEAD changed; use `forkspace land {}` to preserve commit history",
            workspace.name
        );
    }
    if !args.quiet {
        println!("{review}");
        println!("applying…");
    }
    let journal = if plan.changes.is_empty() {
        None
    } else {
        Some(merge::execute(&plan)?)
    };
    Ok(Some(Applied {
        journal,
        counts: ChangeCounts::of(&plan.changes),
    }))
}

/// Package install and optional commit in the base. These run before the journal is closed,
/// so a failure leaves the merge resumable.
fn run_post_merge_steps(args: &MergeArgs, workspace: &Workspace) -> Result {
    if !args.no_install {
        if let Some(manager) = install::detect_package_manager(workspace) {
            crate::util::trace::time("install", || install::run_install(&workspace.base, manager))?;
            println!("ran package install");
        }
    }
    if args.commit || args.message.is_some() {
        let default_message = format!(
            "forkspace merge: applied changes from workspace {}",
            workspace.name
        );
        merge::git_commit(
            &workspace.base,
            args.message.as_deref().unwrap_or(&default_message),
        )?;
    }
    Ok(())
}

/// `merge --undo [NAME]`: put the base back as it was before the latest merge (of `NAME`),
/// or with `--list` show the merges that can be undone.
fn undo(args: &MergeArgs) -> Result {
    if args.list {
        let records = undo::list();
        if records.is_empty() {
            println!("no merges to undo");
        }
        for (_, record) in records {
            println!(
                "{}  {}  {} files  {}",
                record.merged_at,
                record.workspace,
                record.steps.len(),
                record.project.display()
            );
        }
        return Ok(());
    }
    // The state lock keeps a merge from running into the project while it is restored.
    let report = State::with_state(|_| undo::undo(args.name.as_deref()))?;
    println!(
        "undid the merge of '{}' into {}: restored {} files, removed {} added files",
        report.workspace,
        report.project.display(),
        report.restored,
        report.removed
    );
    if !report.changed_since.is_empty() {
        eprintln!(
            "warning: left alone because they changed after the merge (their pre-merge versions stay in the undo store): {}",
            join_paths(&report.changed_since)
        );
    }
    if !report.newer_merge.is_empty() {
        eprintln!(
            "warning: left alone because a newer merge also changed them; undo that merge first: {}",
            join_paths(&report.newer_merge)
        );
    }
    if !args.no_install && report.restored + report.removed > 0 {
        if let Some(manager) = install::detect_in(&report.project) {
            install::run_install(&report.project, manager)?;
            println!("ran package install");
        }
    }
    Ok(())
}

/// The rendered plan followed by its review signals, if any.
fn review_plan(workspace: &Workspace, plan: &MergePlan) -> String {
    let mut rendered = plan::render(plan);
    if let Some(line) = signals::render(&signals::collect(workspace, &plan.changes)) {
        rendered.push('\n');
        rendered.push_str(&line);
    }
    rendered
}
