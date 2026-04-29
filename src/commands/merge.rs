use crate::changes::ChangeKind;
use crate::cli::MergeArgs;
use crate::error::Result;
use crate::state::State;
use crate::sync::journal::{self, OperationPhase};
use crate::sync::merge;
use crate::sync::plan as merge_plan;
use crate::workspace::guard;

pub fn exec(args: MergeArgs) -> Result {
    State::with_state(|state| {
        guard::ensure_no_interrupted_pull(&args.name)?;
        if !args.resume {
            guard::ensure_no_interrupted_merge(&args.name)?;
        }
        let workspace = state.require_workspace(&args.name)?.clone();
        guard::require_view(&workspace)?;

        if args.patch {
            let plan = merge_plan::build(&workspace, true)?;
            if plan.changes.is_empty() {
                println!("nothing to merge in '{}'", args.name);
                return Ok(());
            }
            print!("{}", merge::generate_patch(&workspace, &plan.changes)?);
            if args.drop {
                crate::workspace::remove(state, &args.name)?;
                eprintln!("removed '{}'", args.name);
            }
            return Ok(());
        }

        let overwrite = args.overwrite_base || args.force;
        let (plan, journal_path) = if args.resume {
            (None, Some(merge_plan::resume(&workspace)?))
        } else {
            let plan = merge_plan::build(&workspace, overwrite || args.dry_run)?;
            let review = review_plan(&workspace, &plan);
            if args.dry_run {
                println!("{review}");
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
            if !args.quiet {
                println!("{review}");
                println!("applying…");
            }
            let journal_path = if plan.changes.is_empty() {
                None
            } else {
                Some(merge_plan::execute(&plan)?)
            };
            (Some(plan), journal_path)
        };

        if !args.no_install {
            if let Some(pm) = merge::detect_package_manager(&workspace) {
                merge::run_install(&workspace.base, &pm)?;
                println!("ran package install");
            }
        }
        if args.commit || args.message.is_some() {
            let default_msg = format!("forkspace merge: applied changes from workspace {}", args.name);
            merge::git_commit(
                &workspace.base,
                args.message.as_deref().unwrap_or(&default_msg),
            )?;
        }
        if let Some(path) = &journal_path {
            journal::set_phase(path, OperationPhase::Done)?;
            journal::remove(path)?;
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
            crate::workspace::remove(state, &args.name)?;
        }
        println!("merged {total} files ({added} added, {modified} modified, {deleted} deleted)");
        if args.drop {
            println!("removed '{}'", args.name);
        }
        Ok(())
    })
}

fn review_plan(workspace: &crate::workspace::Workspace, plan: &merge_plan::MergePlan) -> String {
    let mut rendered = merge_plan::render(plan);
    let signals = crate::review::signals::collect(workspace, &plan.changes);
    if let Some(line) = crate::review::signals::render(&signals) {
        rendered.push('\n');
        rendered.push_str(&line);
    }
    rendered
}
