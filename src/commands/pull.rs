use crate::cli::PullArgs;
use crate::error::{ForkspaceError, Result};
use crate::merge;
use crate::pull;
use crate::state::State;

pub fn exec(args: PullArgs) -> Result {
    let name = args
        .name
        .or_else(|| std::env::var("FORKSPACE_WORKSPACE").ok())
        .ok_or_else(|| {
            ForkspaceError::InvalidArgument(
                "workspace name required (pass one or run inside `forkspace enter`)".to_string(),
            )
        })?;

    State::with_state(|state| {
        let workspace = state.require_workspace(&name)?.clone();
        let (path, count) = if args.resume {
            let path = pull::resume_apply(&workspace)?;
            (path, None)
        } else {
            pull::guard(&name)?;
            let plan = pull::build(&workspace, args.theirs)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&plan)
                        .map_err(|error| ForkspaceError::Serialization(error.to_string()))?
                );
            } else {
                println!("{}", pull::render(&plan));
            }
            if args.dry_run {
                return Ok(());
            }
            let count = plan.changes.len();
            (pull::start(&workspace, &plan)?, Some(count))
        };

        if !args.no_install {
            if let Some(manager) = merge::detect_package_manager(&workspace) {
                merge::run_install(&workspace.view_path, &manager)?;
                println!("ran package install in workspace");
            }
        }
        pull::finish(state.require_workspace_mut(&name)?, &path)?;
        // Make the new ancestor durable before removing the recovery journal.
        state.save()?;
        pull::complete(&path)?;
        match count {
            Some(count) => println!("pulled {count} files into '{name}' and re-baselined"),
            None => println!("resumed pull into '{name}' and re-baselined"),
        }
        Ok(())
    })
}
