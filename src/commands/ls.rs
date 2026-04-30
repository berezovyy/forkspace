//! `forkspace ls`: list workspaces as a table, bare names, or JSON.

use std::path::Path;

use owo_colors::OwoColorize;

use crate::backend::{select_backend, ChangeCount};
use crate::cli::LsArgs;
use crate::error::Result;
use crate::state::State;
use crate::workspace::Workspace;

const HEADERS: [&str; 6] = ["NAME", "BASE", "BRANCH", "BACKEND", "CHANGED", "PROCESS"];
const NAME_COLUMN: usize = 0;
const BRANCH_COLUMN: usize = 2;

type Row = [String; HEADERS.len()];

pub fn exec(args: LsArgs) -> Result {
    let state = State::load()?;
    if args.names {
        let mut names: Vec<&String> = state.workspaces.keys().collect();
        names.sort();
        for name in names {
            println!("{name}");
        }
        return Ok(());
    }

    let mut workspaces: Vec<&Workspace> = state.workspaces.values().collect();
    workspaces.sort_by(|a, b| a.name.cmp(&b.name));
    if args.json {
        let values: Vec<_> = workspaces
            .iter()
            .map(|workspace| json_entry(workspace))
            .collect();
        return super::print_json(&values);
    }
    if workspaces.is_empty() {
        println!("no workspaces");
        return Ok(());
    }

    let rows = workspaces
        .iter()
        .map(|workspace| table_row(workspace, args.no_gitignore))
        .collect::<Result<Vec<_>>>()?;
    let show_branch = workspaces
        .iter()
        .any(|workspace| workspace.branch.is_some());
    print_table(&rows, show_branch);
    Ok(())
}

fn json_entry(workspace: &Workspace) -> serde_json::Value {
    serde_json::json!({
        "name": workspace.name,
        "base": workspace.base,
        "parent": workspace.parent,
        "branch": workspace.branch,
        "backend": workspace.backend_kind.as_str(),
        "degraded": workspace.degraded,
        "probe_reason": workspace.probe_reason,
        "base_missing": !workspace.base.is_dir(),
        "view_missing": !workspace.view_path.is_dir(),
    })
}

fn table_row(workspace: &Workspace, include_ignored: bool) -> Result<Row> {
    let count =
        select_backend(Some(workspace.backend_kind))?.change_count(workspace, include_ignored)?;
    Ok([
        workspace.name.clone(),
        origin(workspace),
        workspace.branch.clone().unwrap_or_else(|| "-".to_string()),
        workspace.backend_kind.as_str().to_string(),
        describe_count(count),
        describe_process(workspace),
    ])
}

/// The parent workspace's name for stacked workspaces, otherwise the base directory.
fn origin(workspace: &Workspace) -> String {
    let mut origin = match &workspace.parent {
        Some(parent) => parent.clone(),
        None => shorten_path(&workspace.base),
    };
    if !workspace.base.is_dir() {
        origin.push_str(" (missing)");
    }
    origin
}

/// `path` relative to the current directory as `./...` when it is inside it.
fn shorten_path(path: &Path) -> String {
    let relative = std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf));
    match relative {
        Some(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Some(relative) => format!("./{}", relative.display()),
        None => path.display().to_string(),
    }
}

fn describe_count(count: ChangeCount) -> String {
    match count {
        ChangeCount::Ready(1) => "1 file".to_string(),
        ChangeCount::Ready(count) => format!("{count} files"),
        ChangeCount::Pending => "-".to_string(),
        ChangeCount::Degraded => "degraded".to_string(),
    }
}

fn describe_process(workspace: &Workspace) -> String {
    match &workspace.process {
        Some(process)
            if crate::process::watch::is_alive(process.pid, process.identity.as_deref()) =>
        {
            format!(":{} ({})", process.port, process.command)
        }
        Some(_) => "stopped".to_string(),
        None => "-".to_string(),
    }
}

/// Print left-aligned columns separated by two spaces; the last column is not padded. The
/// BRANCH column is shown only when some workspace has a branch.
fn print_table(rows: &[Row], show_branch: bool) {
    let columns: Vec<usize> = (0..HEADERS.len())
        .filter(|&column| show_branch || column != BRANCH_COLUMN)
        .collect();
    let last = columns.len() - 1;
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(position, &column)| {
            if position == last {
                return 0;
            }
            rows.iter()
                .map(|row| row[column].len())
                .max()
                .unwrap_or(0)
                .max(HEADERS[column].len())
        })
        .collect();
    let line = |cell: &dyn Fn(usize, usize) -> String| {
        columns
            .iter()
            .zip(&widths)
            .map(|(&column, &width)| cell(column, width))
            .collect::<Vec<_>>()
            .join("  ")
    };

    // Styled values are padded before rendering so escape codes do not count toward widths.
    println!(
        "{}",
        line(&|column, width| format!("{:<width$}", HEADERS[column].bold()))
    );
    for row in rows {
        println!(
            "{}",
            line(&|column, width| match column {
                NAME_COLUMN => format!("{:<width$}", row[column].bold().bright_white()),
                _ => format!("{:<width$}", row[column]),
            })
        );
    }
}
