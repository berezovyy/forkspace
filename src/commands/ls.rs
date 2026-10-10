//! `forkspace ls`: list workspaces as a table, bare names, or JSON.

use std::path::Path;

use owo_colors::OwoColorize;

use crate::backend::{select_backend, ChangeCount};
use crate::cli::LsArgs;
use crate::disk;
use crate::error::Result;
use crate::state::State;
use crate::workspace::Workspace;

const HEADERS: [&str; 7] = [
    "NAME", "BASE", "BRANCH", "BACKEND", "CHANGED", "SIZE", "PROCESS",
];
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
    let processes = crate::process::members::counts(workspaces.iter().copied());
    let count_of = |workspace: &Workspace| processes.get(&workspace.name).copied().unwrap_or(0);
    if args.json {
        let values: Vec<_> = workspaces
            .iter()
            .map(|workspace| json_entry(workspace, count_of(workspace)))
            .collect();
        return super::print_json(&values);
    }
    if workspaces.is_empty() {
        println!("no workspaces");
        return Ok(());
    }

    let rows = workspaces
        .iter()
        .map(|workspace| table_row(workspace, args.no_gitignore, count_of(workspace)))
        .collect::<Result<Vec<_>>>()?;
    let show_branch = workspaces
        .iter()
        .any(|workspace| workspace.branch.is_some());
    print_table(&rows, show_branch);
    print_total(&workspaces);
    Ok(())
}

/// `forkspace uses 2.1 GB (workspaces 0.8 GB, frozen copies 0.9 GB, …)`.
fn print_total(workspaces: &[&Workspace]) {
    let Ok(stores) = disk::StoreUsage::measure() else {
        return;
    };
    let in_workspaces: u64 = workspaces
        .iter()
        .filter_map(|workspace| disk::workspace_bytes(workspace).ok())
        .sum();
    let mut parts = vec![format!("workspaces {}", disk::human(in_workspaces))];
    for (label, bytes) in [
        ("frozen copies", stores.frozen),
        ("undo and checkpoints", stores.history),
        ("other", stores.other),
    ] {
        if bytes > 0 {
            parts.push(format!("{label} {}", disk::human(bytes)));
        }
    }
    let total = in_workspaces + stores.frozen + stores.history + stores.other;
    println!(
        "\nforkspace uses {} ({})",
        disk::human(total),
        parts.join(", ")
    );
}

fn json_entry(workspace: &Workspace, processes: usize) -> serde_json::Value {
    serde_json::json!({
        "name": workspace.name,
        "base": workspace.base,
        "parent": workspace.parent,
        "branch": workspace.branch,
        "backend": workspace.backend_kind.as_str(),
        "live": workspace.is_live(),
        "degraded": workspace.degraded,
        "probe_reason": workspace.probe_reason,
        "base_missing": !workspace.base.is_dir(),
        "view_missing": !workspace.view_path.is_dir(),
        "size_bytes": disk::workspace_bytes(workspace).ok(),
        "processes": processes,
        "memory_bytes": crate::process::systemd::memory_current(&workspace.name),
        "limits": workspace.limits,
    })
}

fn table_row(workspace: &Workspace, include_ignored: bool, processes: usize) -> Result<Row> {
    let count =
        select_backend(Some(workspace.backend_kind))?.change_count(workspace, include_ignored)?;
    Ok([
        workspace.name.clone(),
        origin(workspace),
        workspace.branch.clone().unwrap_or_else(|| "-".to_string()),
        describe_backend(workspace),
        describe_count(count),
        disk::workspace_bytes(workspace).map_or_else(|_| "?".to_string(), disk::human),
        describe_process(workspace, processes),
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

/// The backend kind, marked when the workspace sees later base changes (`new --live`).
fn describe_backend(workspace: &Workspace) -> String {
    let kind = workspace.backend_kind.as_str();
    if workspace.is_live() {
        format!("{kind} (live)")
    } else {
        kind.to_string()
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

/// The `forkspace run` dev server, if any, and how many processes run in the workspace.
fn describe_process(workspace: &Workspace, processes: usize) -> String {
    let server = match &workspace.process {
        Some(process)
            if crate::process::watch::is_alive(process.pid, process.identity.as_deref()) =>
        {
            Some(format!(":{} ({})", process.port, process.command))
        }
        Some(_) => Some("stopped".to_string()),
        None => None,
    };
    let memory = crate::process::systemd::memory_current(&workspace.name)
        .filter(|bytes| *bytes > 0)
        .map(|bytes| format!("{:.1} MiB", bytes as f64 / 1_048_576.0));
    let count = match (processes, memory) {
        (0, _) => None,
        (1, None) => Some("1 process".to_string()),
        (1, Some(memory)) => Some(format!("1 process, {memory}")),
        (n, None) => Some(format!("{n} processes")),
        (n, Some(memory)) => Some(format!("{n} processes, {memory}")),
    };
    match (server, count) {
        (Some(server), Some(count)) => format!("{server}, {count}"),
        (Some(text), None) | (None, Some(text)) => text,
        (None, None) => "-".to_string(),
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
