//! `forkspace pull`: bring base-side changes into a workspace and re-baseline it.

use crate::cli::PullArgs;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::sync::{install, pull};
use crate::workspace::{guard, Workspace};

pub fn exec(args: PullArgs) -> Result {
    let name = workspace_name(args.name)?;
    State::with_state(|state| {
        guard::ensure_no_interrupted_merge(&name)?;
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;

        // `count` is `None` when resuming, where the original plan is no longer known.
        let (journal, count) = if args.resume {
            (pull::resume_apply(&workspace)?, None)
        } else {
            guard::ensure_no_interrupted_pull(&name)?;
            let plan = pull::build(&workspace, args.theirs)?;
            print_plan(&plan, args.json)?;
            if args.dry_run {
                return Ok(());
            }
            (pull::start(&workspace, &plan)?, Some(plan.changes.len()))
        };

        if !args.no_install {
            install_in_workspace(&workspace)?;
        }
        pull::finish(state.require_workspace_mut(&name)?, &journal)?;
        // Make the new ancestor durable before removing the recovery journal.
        state.save()?;
        pull::complete(&journal)?;
        match count {
            Some(count) => println!("pulled {count} files into '{name}' and re-baselined"),
            None => println!("resumed pull into '{name}' and re-baselined"),
        }
        Ok(())
    })
}

/// The named workspace, or the one this shell was entered into.
fn workspace_name(name: Option<String>) -> Result<String> {
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
