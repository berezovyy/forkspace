//! `forkspace pull`: bring base-side changes into a workspace and re-baseline it.

use crate::cli::PullArgs;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::sync::journal::{self, OperationPhase};
use crate::sync::{install, pull};
use crate::workspace::{guard, Workspace};

pub fn exec(args: PullArgs) -> Result {
    let name = workspace_name(args.name)?;
    if !args.resume {
        crate::changes::manifest::settle_now(&name, true)?;
    }
    State::with_state(|state| {
        guard::ensure_no_interrupted_merge(&name)?;
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;

        // `count` is `None` when resuming, where the original plan is no longer known; the
        // install then runs, since the plan may have changed the dependencies.
        let (mut journal, count, marked, mut dependencies_changed) = if args.resume {
            (pull::resume_apply(&workspace)?, None, Vec::new(), true)
        } else {
            guard::ensure_no_interrupted_pull(&name)?;
            let plan = pull::build_with(&workspace, args.theirs, !args.no_install)?;
            print_plan(&plan, args.json)?;
            if args.dry_run {
                pull::discard_staging(&workspace);
                return Ok(());
            }
            // Pull rewrites workspace files; keep a way back.
            crate::sync::checkpoint::create(
                &workspace,
                Some("before pull".to_string()),
                false,
                true,
            )?;
            let dependencies_changed = install::touches_dependencies(
                plan.changes
                    .iter()
                    .map(|change| &change.path)
                    .chain(&plan.auto_merged)
                    .chain(&plan.marked),
            );
            let journal = pull::start(&workspace, &plan)?;
            (
                journal,
                Some(plan.changes.len()),
                plan.marked,
                dependencies_changed,
            )
        };

        // Base changes made after the plan (while it applied, or while it was interrupted)
        // are pulled before re-baselining, or the new fork point would hold them and the
        // view not, and they would look like the workspace's reversions (PULL-10). A few
        // rounds at most: `finish` leaves whatever changes later for the next pull.
        let mut extra = 0;
        for _ in 0..CATCH_UP_ROUNDS {
            let previous = journal::load(&journal)?;
            let open = matches!(
                previous.phase,
                OperationPhase::Planned | OperationPhase::Applying | OperationPhase::Applied
            );
            if !open || !pull::base_moved_since(&workspace, previous.guard.as_ref()) {
                break;
            }
            let plan = pull::build_since(&workspace, &previous, !args.no_install)?;
            if !plan.changes.is_empty() {
                println!(
                    "the base changed while pulling; pulling {} more files",
                    plan.changes.len()
                );
            }
            dependencies_changed |= install::touches_dependencies(
                plan.changes
                    .iter()
                    .map(|change| &change.path)
                    .chain(&plan.auto_merged),
            );
            extra += plan.changes.len();
            journal = pull::start_round(&workspace, &plan, Some(&previous))?;
        }
        let held = journal::held(&journal)?;

        if !args.no_install && dependencies_changed {
            install_in_workspace(&workspace)?;
        }
        let recorded = state.require_workspace_mut(&name)?;
        let left = pull::finish(recorded, &journal)?;
        for path in &marked {
            if !recorded.unresolved_conflicts.contains(path) {
                recorded.unresolved_conflicts.push(path.clone());
            }
        }
        let unresolved = recorded.unresolved_conflicts.clone();
        // Make the new ancestor durable before removing the recovery journal.
        state.save()?;
        pull::complete(&journal)?;
        match count {
            // What was held was not pulled (MERGE-37).
            Some(count) => println!(
                "pulled {} files into '{name}' and re-baselined",
                (count + extra).saturating_sub(held.len())
            ),
            None => println!("resumed pull into '{name}' and re-baselined"),
        }
        if !left.is_empty() {
            eprintln!(
                "warning: the base changed while pulling; left for the next pull: {}",
                crate::sync::join_paths(&left)
            );
        }
        let held = (!held.is_empty()).then(|| ForkspaceError::ChangedDuring {
            operation: "pull".to_string(),
            paths: crate::sync::display_paths(&held),
            remedy: "they changed while the pull ran, so the base's versions were not pulled into them; the workspace's versions now count as its own edits".to_string(),
        });
        if marked.is_empty() {
            return held.map_or(Ok(()), Err);
        }
        if let Some(held) = held {
            eprintln!("warning: {held}");
        }
        Err(ForkspaceError::ConflictMarkers {
            workspace: name.clone(),
            paths: crate::sync::display_paths(&unresolved),
        })
    })
}

/// Catch-up rounds a pull runs at most for base changes made while it applies.
const CATCH_UP_ROUNDS: usize = 3;

/// The named workspace, or the one this shell was entered into.
pub(crate) fn workspace_name(name: Option<String>) -> Result<String> {
    name.or_else(|| std::env::var("FORKSPACE_WORKSPACE").ok())
        .ok_or_else(|| {
            ForkspaceError::InvalidArgument(
                "workspace name required (pass one or run inside `forkspace enter`)".to_string(),
            )
        })
}

fn print_plan(plan: &pull::PullPlan, json: bool) -> Result {
    if json {
        super::print_json(plan)
    } else {
        println!("{}", pull::render(plan));
        Ok(())
    }
}

fn install_in_workspace(workspace: &Workspace) -> Result {
    if let Some(manager) = install::detect_package_manager(workspace) {
        install::run_install(&workspace.view_path, manager)?;
        println!("ran package install in workspace");
    }
    Ok(())
}
