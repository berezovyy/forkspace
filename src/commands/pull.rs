//! `forkspace pull`: bring base-side changes into a workspace and re-baseline it.

use crate::cli::PullArgs;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::sync::journal::{self, OperationPhase};
use crate::sync::{install, pull};
use crate::workspace::{guard, Workspace};

pub fn exec(args: PullArgs) -> Result {
    let name = workspace_name(args.name)?;
    // A pull into this workspace that was interrupted is finished first.
    let resume = journal::find_open(journal::OperationKind::Pull, &name)?.is_some();
    if !resume {
        crate::changes::manifest::settle_now(&name, true)?;
    }
    // With --json, stdout holds only the plan; messages go to stderr.
    let say = |message: String| {
        if args.json {
            eprintln!("{message}");
        } else {
            println!("{message}");
        }
    };
    State::with_state(|state| {
        guard::ensure_no_interrupted_merge(&name)?;
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;

        // `count` is `None` when resuming, where the original plan is no longer known; the
        // install then runs, since the plan may have changed the dependencies.
        let mut taken = Vec::new();
        let mut line_merged = Vec::new();
        let mut before = None;
        let (mut journal, count, marked, mut dependencies_changed) = if resume {
            (pull::resume_apply(&workspace)?, None, Vec::new(), true)
        } else {
            let plan = pull::build_with(&workspace, true)?;
            if args.json {
                super::print_json(&plan)?;
            }
            // Pull rewrites workspace files; keep a way back.
            before = Some(
                crate::sync::checkpoint::create(
                    &workspace,
                    Some("before pull".to_string()),
                    false,
                    true,
                )?
                .id,
            );
            taken.clone_from(&plan.conflicts);
            line_merged.clone_from(&plan.auto_merged);
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
            let plan = pull::build_since(&workspace, &previous, true)?;
            if !plan.changes.is_empty() {
                say(format!(
                    "the base changed while pulling; pulling {} more files",
                    plan.changes.len()
                ));
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

        if dependencies_changed {
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
            Some(count) => say(format!(
                "pulled {} files into '{name}'",
                (count + extra).saturating_sub(held.len())
            )),
            None => say(format!("finished the interrupted pull into '{name}'")),
        }
        if !line_merged.is_empty() {
            say(format!(
                "merged line by line: {}",
                crate::sync::join_paths(&line_merged)
            ));
        }
        if !taken.is_empty() {
            let checkpoint = before.map_or_else(String::new, |id| {
                format!("; yours are in snapshot {id} (`forkspace restore {name} {id}`)")
            });
            say(format!(
                "took the project's version of {}, which cannot be merged line by line{checkpoint}",
                crate::sync::join_paths(&taken)
            ));
        }
        if !left.is_empty() {
            eprintln!(
                "warning: your project changed while pulling; left for the next pull: {}",
                crate::sync::join_paths(&left)
            );
        }
        let held = (!held.is_empty()).then(|| ForkspaceError::ChangedDuring {
            operation: "pull".to_string(),
            paths: crate::sync::display_paths(&held),
            remedy: "they changed while the pull ran, so your project's versions were not pulled into them; the workspace's versions now count as its own edits".to_string(),
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

/// The named workspace, or the one you are in (see [`crate::workspace::pick`]).
pub(crate) fn workspace_name(name: Option<String>) -> Result<String> {
    crate::workspace::pick(name)
}

fn install_in_workspace(workspace: &Workspace) -> Result {
    if let Some(manager) = install::detect_package_manager(workspace) {
        install::run_install(&workspace.view_path, manager)?;
        println!("ran package install in workspace");
    }
    Ok(())
}
