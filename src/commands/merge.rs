use crate::cli::MergeArgs;
use crate::commands::drop as drop_cmd;
use crate::diff::ChangeKind;
use crate::error::Result;
use crate::merge;
use crate::merge_plan;
use crate::state::State;

pub fn exec(args: MergeArgs) -> Result {
    State::with_state(|state| {
        crate::pull::guard(&args.name)?;
        let workspace = state.require_workspace(&args.name)?.clone();

        if args.patch {
            let plan = merge_plan::build(&workspace, true)?;
            if plan.changes.is_empty() {
                println!("nothing to merge in '{}'", args.name);
                return Ok(());
            }
            print!("{}", merge::generate_patch(&workspace, &plan.changes)?);
            if args.drop {
                drop_cmd::remove_workspace(state, &args.name)?;
                eprintln!("removed '{}'", args.name);
            }
            return Ok(());
        }

        let overwrite = args.overwrite_base || args.force;
        let (plan, journal) = if args.resume {
            (None, Some(merge_plan::resume(&workspace)?))
        } else {
            let plan = merge_plan::build(&workspace, overwrite || args.dry_run)?;
            if args.dry_run {
                println!("{}", merge_plan::render(&plan));
                return Ok(());
            }
            if overwrite && !plan.conflicts.is_empty() {
                eprintln!(
                    "warning: overwriting base changes: {}",
                    plan.conflicts
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if merge::workspace_head_changed(&workspace)? {
                eprintln!(
                    "warning: workspace git HEAD changed; use `forkspace land {}` to preserve commit history",
                    workspace.name
                );
            }
            let journal = if plan.changes.is_empty() {
                None
            } else {
                Some(merge_plan::execute(&plan)?)
            };
            (Some(plan), journal)
        };

        if !args.no_install {
            if let Some(pm) = merge::detect_package_manager(&workspace) {
                merge::run_install(&workspace.base, &pm)?;
                println!("ran package install");
            }
        }
        if args.commit || args.message.is_some() {
            let default_msg = format!(
                "forkspace merge: applied changes from workspace {}",
                args.name
            );
            merge::git_commit(
                &workspace.base,
                args.message.as_deref().unwrap_or(&default_msg),
            )?;
        }
        if let Some(journal) = &journal {
            merge_plan::finish(journal)?;
        }

        let (added, modified, deleted) = plan.as_ref().map_or((0, 0, 0), |plan| {
            (
                plan.changes
                    .iter()
                    .filter(|change| matches!(change.kind, ChangeKind::Added))
                    .count(),
                plan.changes
                    .iter()
                    .filter(|change| matches!(change.kind, ChangeKind::Modified))
                    .count(),
                plan.changes
                    .iter()
                    .filter(|change| matches!(change.kind, ChangeKind::Deleted))
                    .count(),
            )
        });
        let total = added + modified + deleted;
        if total == 0 && !args.resume {
            println!("nothing to merge in '{}'", args.name);
            return Ok(());
        }
        if args.drop {
            drop_cmd::remove_workspace(state, &args.name)?;
        }
        println!("merged {total} files ({added} added, {modified} modified, {deleted} deleted)");
        if args.drop {
            println!("removed '{}'", args.name);
        }
        Ok(())
    })
}
