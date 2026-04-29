use crate::backend::select_backend;
use crate::changes::filter::ChangeFilter;
use crate::changes::manifest::ManifestStatus;
use crate::changes::{
    collect_cumulative_changes, collect_cumulative_changes_fast, format_diff,
    generate_unified_diff, DiffFormat, DiffOutput,
};
use crate::cli::DiffArgs;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::workspace::guard;

pub fn exec(args: DiffArgs) -> Result {
    guard::ensure_idle(&args.name)?;
    let state = State::load()?;
    let workspace = state.require_workspace(&args.name)?;
    guard::require_view(workspace)?;

    let need_line_counts = !args.stat && !args.files;
    let changes = if args.cumulative {
        if need_line_counts {
            collect_cumulative_changes(workspace, &state)?
        } else {
            collect_cumulative_changes_fast(workspace, &state)?
        }
    } else {
        select_backend(Some(workspace.backend_kind))?.changes(
            workspace,
            args.no_gitignore,
            need_line_counts,
        )?
    };

    let changes = if args.no_gitignore || !args.cumulative {
        changes
    } else {
        ChangeFilter::build(&workspace.base).filter_changes(changes)
    };
    let signals = crate::review::signals::collect(workspace, &changes);

    let manifest_status = State::load()?
        .workspaces
        .get(&workspace.name)
        .map_or(workspace.manifest_status, |workspace| {
            workspace.manifest_status
        });
    match manifest_status {
        ManifestStatus::Building => {
            eprintln!("warning: deletions pending manifest build");
        }
        ManifestStatus::Degraded => {
            eprintln!("warning: deletion tracking degraded; the fork manifest is unavailable");
        }
        ManifestStatus::Absent | ManifestStatus::Ready => {}
    }

    if changes.is_empty() {
        if args.json {
            let output = DiffOutput {
                workspace: args.name.clone(),
                base: workspace.base.clone(),
                changes: Vec::new(),
                signals,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&output)
                    .map_err(|e| ForkspaceError::Serialization(e.to_string()))?
            );
        } else {
            println!("clean — no changes");
        }
        return Ok(());
    }

    if args.json {
        let output = DiffOutput {
            workspace: args.name.clone(),
            base: workspace.base.clone(),
            changes,
            signals,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|e| ForkspaceError::Serialization(e.to_string()))?
        );
    } else if args.full {
        for change in &changes {
            print!("{}", generate_unified_diff(workspace, change));
        }
    } else {
        let fmt = if args.files {
            DiffFormat::Files
        } else if args.stat {
            DiffFormat::Stat
        } else {
            DiffFormat::Default
        };
        println!("{}", format_diff(&changes, &fmt));
        if args.stat {
            if let Some(line) = crate::review::signals::render(&signals) {
                println!("{line}");
            }
        }
    }

    Ok(())
}
