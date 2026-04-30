//! `forkspace diff`: show what changed in a workspace.

use crate::backend::select_backend;
use crate::changes::filter::ChangeFilter;
use crate::changes::manifest::ManifestStatus;
use crate::changes::{
    collect_cumulative_changes, collect_cumulative_changes_fast, format_diff,
    generate_unified_diff, DiffFormat, DiffOutput, FileChange,
};
use crate::cli::DiffArgs;
use crate::error::Result;
use crate::review;
use crate::state::State;
use crate::workspace::{guard, Workspace};

pub fn exec(args: DiffArgs) -> Result {
    guard::ensure_idle(&args.name)?;
    let state = State::load()?;
    let workspace = state.require_workspace(&args.name)?;
    guard::require_view(workspace)?;

    let changes = collect(&args, workspace, &state)?;
    let signals = review::signals::collect(workspace, &changes);
    warn_about_manifest(manifest_status_after_scan(workspace)?);

    if args.json {
        let output = DiffOutput {
            workspace: args.name.clone(),
            base: workspace.base.clone(),
            changes,
            signals,
        };
        return super::print_json(&output);
    }
    if changes.is_empty() {
        println!("clean — no changes");
        return Ok(());
    }
    if args.full {
        for change in &changes {
            print!("{}", generate_unified_diff(workspace, change));
        }
        return Ok(());
    }

    let format = if args.files {
        DiffFormat::Files
    } else if args.stat {
        DiffFormat::Stat
    } else {
        DiffFormat::Default
    };
    println!("{}", format_diff(&changes, &format));
    if args.stat {
        if let Some(line) = review::signals::render(&signals) {
            println!("{line}");
        }
    }
    Ok(())
}

fn collect(args: &DiffArgs, workspace: &Workspace, state: &State) -> Result<Vec<FileChange>> {
    let need_line_counts = !args.stat && !args.files;
    if !args.cumulative {
        return select_backend(Some(workspace.backend_kind))?.changes(
            workspace,
            args.no_gitignore,
            need_line_counts,
        );
    }

    let changes = if need_line_counts {
        collect_cumulative_changes(workspace, state)?
    } else {
        collect_cumulative_changes_fast(workspace, state)?
    };
    // Cumulative layers are collected unfiltered, so apply ignore rules here.
    if args.no_gitignore {
        return Ok(changes);
    }
    Ok(ChangeFilter::build(&workspace.base).filter_changes(changes))
}

/// Scanning a snapshot can change its manifest status (an unreadable manifest is degraded
/// or rebuilt), so read the status back from disk rather than trusting the earlier load.
fn manifest_status_after_scan(workspace: &Workspace) -> Result<ManifestStatus> {
    Ok(State::load()?
        .workspaces
        .get(&workspace.name)
        .map_or(workspace.manifest_status, |current| current.manifest_status))
}

fn warn_about_manifest(status: ManifestStatus) {
    match status {
        ManifestStatus::Building => eprintln!("warning: deletions pending manifest build"),
        ManifestStatus::Degraded => {
            eprintln!("warning: deletion tracking degraded; the fork manifest is unavailable");
        }
        ManifestStatus::Absent | ManifestStatus::Ready => {}
    }
}
