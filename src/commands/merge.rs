//! `forkspace merge`: apply a workspace's changes to its base, or print them as a patch.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::backend::select_backend;
use crate::changes::filter::ChangeFilter;
use crate::changes::{ChangeCounts, FileChange};
use crate::cli::MergeArgs;
use crate::error::Result;
use crate::review::signals;
use crate::state::State;
use crate::sync::journal::{self, OperationPhase};
use crate::sync::plan::{self, MergePlan, PlanOptions};
use crate::sync::{install, is_transferable, join_paths, merge, undo};
use crate::util::fs::ctime_ns;
use crate::util::{now_unix_ns, IoContext};
use crate::workspace::{guard, Workspace};

/// The outcome of the apply step: the journal still to close, what was transferred (zero
/// when resuming, where the original plan is no longer known), and whether the transfer can
/// change the installed dependencies (assumed when resuming, for the same reason).
struct Applied {
    journal: Option<PathBuf>,
    /// The planned changes; empty when resuming.
    changes: Vec<FileChange>,
    dependencies_changed: bool,
    planned: Planned,
}

/// What the merge saw of the workspace: the paths it transferred or left to the base's
/// version, and when it started reading the view.
struct Planned {
    paths: BTreeSet<PathBuf>,
    since_ns: u128,
}

pub fn exec(args: MergeArgs) -> Result {
    if args.undo {
        return undo(&args);
    }
    let name = args.name.clone().unwrap_or_default();
    if !args.resume {
        crate::changes::manifest::settle_now(&name, true)?;
    }
    State::with_state(|state| {
        guard::ensure_no_interrupted_pull(&name)?;
        if !args.resume {
            guard::ensure_no_interrupted_merge(&name)?;
        }
        let workspace = state.require_workspace(&name)?.clone();
        guard::require_live_view(state, &workspace)?;

        if args.patch {
            if args.drop {
                ensure_patch_applies(&workspace)?;
            }
            if let Some(planned) = print_patch(&name, &workspace)? {
                if args.drop {
                    ensure_unchanged_since(&workspace, &planned)?;
                    crate::workspace::remove(state, &name)?;
                    eprintln!("removed '{}'", name);
                }
            }
            return Ok(());
        }

        let Some(applied) = apply(&args, &workspace)? else {
            return Ok(());
        };
        run_post_merge_steps(&args, &workspace, applied.dependencies_changed)?;
        let mut held = Vec::new();
        if let Some(path) = &applied.journal {
            held = journal::held(path)?;
            journal::finish_undo(path)?;
            journal::set_phase(path, OperationPhase::Done)?;
            journal::remove(path)?;
        }

        state
            .require_workspace_mut(&name)?
            .unresolved_conflicts
            .clear();
        if applied.changes.is_empty() && !args.resume {
            println!("nothing to merge in '{}'", name);
            return Ok(());
        }
        // What was held was not merged (MERGE-37).
        let counts = ChangeCounts::of(
            applied
                .changes
                .iter()
                .filter(|change| !held.contains(&change.path)),
        );
        if !held.is_empty() {
            // The workspace still holds the only copy of what was not applied: never drop it.
            if !args.resume {
                println!(
                    "merged {} files; left {} alone that changed while merging",
                    counts.total(),
                    held.len()
                );
            }
            return Err(merge::held_error(&held));
        }
        let summary = format!(
            "merged {} files ({} added, {} modified, {} deleted)",
            counts.total(),
            counts.added,
            counts.modified,
            counts.deleted
        );
        if args.drop {
            if let Err(error) = ensure_unchanged_since(&workspace, &applied.planned) {
                println!("{summary}");
                return Err(error);
            }
            crate::workspace::remove(state, &name)?;
        }
        println!("{summary}");
        if args.drop {
            println!("removed '{}'", name);
        }
        Ok(())
    })
}

/// Refuse to drop a workspace whose view changed while it was merged or exported (MERGE-19):
/// a transferable change the merge did not see, a file written after it started reading the
/// view, or a change it saw that is gone. Those exist only in the workspace.
fn ensure_unchanged_since(workspace: &Workspace, planned: &Planned) -> Result {
    let backend = select_backend(Some(workspace.backend_kind))?;
    let include_ignored = !workspace.backend_kind.stacking_is_snapshot();
    let now: BTreeSet<PathBuf> = ChangeFilter::build(&workspace.base)
        .filter_changes(backend.changes(workspace, include_ignored, false)?)
        .into_iter()
        .filter(|change| is_transferable(change.class))
        .map(|change| change.path)
        .collect();
    let view = backend.view_path(workspace);
    let since = i128::try_from(planned.since_ns).unwrap_or(i128::MAX);
    // A path inside a planned one (a directory deleted or replaced whole) was seen with it.
    let seen = |path: &Path| {
        path.ancestors()
            .any(|ancestor| planned.paths.contains(ancestor))
    };
    let written = |path: &Path| {
        fs::symlink_metadata(view.join(path)).is_ok_and(|metadata| ctime_ns(&metadata) > since)
    };
    let mut changed: Vec<PathBuf> = now
        .iter()
        .filter(|path| !seen(path) || written(path))
        .cloned()
        .collect();
    changed.extend(
        planned
            .paths
            .iter()
            .filter(|path| {
                !now.contains(*path)
                    && !fs::symlink_metadata(view.join(path))
                        .is_ok_and(|metadata| metadata.is_dir())
            })
            .cloned(),
    );
    if changed.is_empty() {
        return Ok(());
    }
    changed.sort();
    changed.dedup();
    Err(crate::error::ForkspaceError::ChangedDuring {
        operation: "merge --drop".to_string(),
        paths: crate::sync::display_paths(&changed),
        remedy: "the workspace changed them while the merge ran, so the base may not have them; the workspace was kept; review them and merge again".to_string(),
    })
}

/// Refuse `merge --patch --drop` unless the patch is sure to apply to the base as it is now:
/// once the workspace is dropped, a patch that `git apply` refuses is the only copy of the
/// work (MERGE-07). A direct merge's refusals refuse here too, and so does a file the base
/// changed since the fork that a direct merge would combine line by line, since the patch
/// carries only the workspace's side.
fn ensure_patch_applies(workspace: &Workspace) -> Result {
    let staged = journal::staging_dir(&journal::path(
        journal::OperationKind::Merge,
        &workspace.name,
        workspace.fork_time_ns,
    ));
    let plan = plan::build(workspace, false);
    let _ = std::fs::remove_dir_all(&staged);
    let plan = plan?;
    // A path the base already holds exactly as the workspace does is left out of the patch
    // (MERGE-36): nothing of it can be lost.
    let combined: Vec<PathBuf> = plan
        .auto_merged
        .iter()
        .chain(&plan.kept_base)
        .filter(|path| !already_in_base(workspace, &plan.source, path))
        .cloned()
        .collect();
    if combined.is_empty() {
        return Ok(());
    }
    Err(crate::error::ForkspaceError::PatchIncomplete {
        workspace: workspace.name.clone(),
        omitted: combined
            .iter()
            .map(|path| {
                format!(
                    "{} (the base changed it too since the fork, so `git apply` may refuse the patch and --drop would lose the change)",
                    path.display()
                )
            })
            .collect(),
    })
}

/// Print the workspace's changes as a Git patch. Returns what it carried, if anything.
/// Fails before printing anything when a change cannot be carried by the patch, so neither a
/// caller nor `--drop` ever acts on a partial one.
fn print_patch(name: &str, workspace: &Workspace) -> Result<Option<Planned>> {
    let since_ns = now_unix_ns();
    let plan = plan::build(workspace, true)?;
    if plan.changes.is_empty() {
        println!("nothing to merge in '{name}'");
        return Ok(None);
    }
    // What the base already holds exactly as the workspace does needs no hunk; one would not
    // apply there (MERGE-36).
    let carried: Vec<_> = plan
        .changes
        .iter()
        .filter(|change| !already_in_base(workspace, &plan.source, &change.path))
        .cloned()
        .collect();
    let patch = merge::generate_patch(workspace, &plan.source, &carried)?;
    let conflicts: Vec<PathBuf> = plan
        .conflicts
        .iter()
        .filter(|path| !already_in_base(workspace, &plan.source, path))
        .cloned()
        .collect();
    if !conflicts.is_empty() {
        eprintln!(
            "warning: the base changed since the fork for {}; the patch holds the workspace's changes against the fork-time versions, so applying it to this base may be refused there",
            join_paths(&conflicts)
        );
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&patch)
        .and_then(|()| stdout.flush())
        .io_context(|| "write patch to stdout".to_string())?;
    Ok(Some(Planned {
        paths: plan.changes.into_iter().map(|change| change.path).collect(),
        since_ns,
    }))
}

/// Whether the base entry at `relative` is already what the workspace has at `source`: the
/// same kind, permissions and content, or both missing.
fn already_in_base(workspace: &Workspace, source: &Path, relative: &Path) -> bool {
    crate::sync::pull::equivalent(&workspace.base.join(relative), &source.join(relative))
}

/// Resume an interrupted merge, or plan, review, and apply a new one.
/// Returns `None` for a dry run, which stops after printing the plan.
fn apply(args: &MergeArgs, workspace: &Workspace) -> Result<Option<Applied>> {
    if args.resume {
        let path = merge::resume(workspace)?;
        let journal = journal::load(&path)?;
        // Paths the plan left to the base's version are not recorded: such a change counts as
        // unseen, which keeps the workspace.
        let planned = Planned {
            paths: journal
                .changes
                .into_iter()
                .map(|change| change.path)
                .collect(),
            since_ns: journal.guard.map_or(0, |guard| guard.planned_ns),
        };
        return Ok(Some(Applied {
            journal: Some(path),
            changes: Vec::new(),
            dependencies_changed: true,
            planned,
        }));
    }

    let overwrite = args.overwrite_base || args.force;
    // Before the plan reads the view: a write after this is not in the merge.
    let since_ns = now_unix_ns();
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
        changes: plan.changes.clone(),
        dependencies_changed: install::touches_dependencies(
            plan.changes.iter().map(|change| &change.path),
        ),
        planned: Planned {
            paths: plan
                .changes
                .iter()
                .map(|change| change.path.clone())
                .chain(plan.kept_base.iter().cloned())
                .collect(),
            since_ns,
        },
    }))
}

/// Package install (only when the merge changed a package manifest, lockfile, or package
/// manager setting) and optional commit in the base. These run before the journal is closed,
/// so a failure leaves the merge resumable.
fn run_post_merge_steps(
    args: &MergeArgs,
    workspace: &Workspace,
    dependencies_changed: bool,
) -> Result {
    if !args.no_install && dependencies_changed {
        if let Some(manager) = install::detect_package_manager(workspace) {
            install::run_install(&workspace.base, manager)?;
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
    if !args.no_install && install::touches_dependencies(&report.reverted) {
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
