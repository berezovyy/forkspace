//! `forkspace ls`: list workspaces as a table, or as JSON for tools.

use std::path::Path;

use crate::util::style::{paint, Style};

use crate::backend::{select_backend, ChangeCount};
use crate::cli::LsArgs;
use crate::disk;
use crate::error::Result;
use crate::state::State;
use crate::workspace::Workspace;

const HEADERS: [&str; 6] = ["NAME", "PROJECT", "CHANGED", "AGE", "SIZE", "RUNNING"];
const NAME_COLUMN: usize = 0;
const PROJECT_COLUMN: usize = 1;

type Row = [String; HEADERS.len()];

pub fn exec(args: LsArgs) -> Result {
    let state = State::load()?;

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

    // Measured once: each is a walk of the whole view.
    let sizes: Vec<Option<u64>> = workspaces
        .iter()
        .map(|workspace| disk::workspace_bytes(workspace).ok())
        .collect();
    let rows = workspaces
        .iter()
        .zip(&sizes)
        .map(|(workspace, size)| table_row(workspace, count_of(workspace), *size))
        .collect::<Result<Vec<_>>>()?;
    // The project column only tells workspaces of different projects apart.
    let projects: std::collections::BTreeSet<&Path> = workspaces
        .iter()
        .map(|workspace| workspace.base.as_path())
        .collect();
    print_table(&rows, projects.len() > 1);
    print_total(sizes.iter().flatten().sum());
    Ok(())
}

/// `forkspace uses 2.1 GB (workspaces 0.8 GB, frozen copies 0.9 GB, …)`.
fn print_total(in_workspaces: u64) {
    let Ok(stores) = disk::StoreUsage::measure() else {
        return;
    };
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
        "age_seconds": age_seconds(workspace),
        "degraded": workspace.degraded,
        "probe_reason": workspace.probe_reason,
        "base_missing": !workspace.base.is_dir(),
        "view_missing": view_missing(workspace),
        "size_bytes": disk::workspace_bytes(workspace).ok(),
        "processes": processes,
        "memory_bytes": crate::process::systemd::memory_current(&workspace.name),
    })
}

fn table_row(workspace: &Workspace, processes: usize, size: Option<u64>) -> Result<Row> {
    // A folder deleted or moved by hand has nothing to count; one row says so, and the rest of
    // the table still shows (STATE-12).
    let count = if view_missing(workspace) {
        "view missing".to_string()
    } else {
        match select_backend(Some(workspace.backend_kind))?.change_count(workspace, false) {
            Ok(count) => describe_count(count),
            Err(error) => {
                eprintln!(
                    "warning: cannot count the changes of '{}': {error}",
                    workspace.name
                );
                "?".to_string()
            }
        }
    };
    Ok([
        workspace.name.clone(),
        origin(workspace),
        count,
        describe_age(age_seconds(workspace)),
        size.map_or_else(|| "?".to_string(), disk::human),
        describe_process(workspace, processes),
    ])
}

/// Seconds since the workspace was made.
fn age_seconds(workspace: &Workspace) -> u64 {
    let now = crate::util::now_unix_ns();
    u64::try_from(now.saturating_sub(workspace.fork_time_ns) / 1_000_000_000).unwrap_or(u64::MAX)
}

/// `45s`, `12m`, `5h`, `3d`.
fn describe_age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Whether the workspace's folder is gone (deleted or moved by hand).
fn view_missing(workspace: &Workspace) -> bool {
    !workspace.view_path.is_dir()
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
        ChangeCount::Ready(0) => "nothing".to_string(),
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
/// PROJECT column is shown only when the workspaces come from more than one project.
fn print_table(rows: &[Row], show_project: bool) {
    let columns: Vec<usize> = (0..HEADERS.len())
        .filter(|&column| show_project || column != PROJECT_COLUMN)
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

    // Values are padded before they are styled, so escape codes do not count toward widths.
    println!(
        "{}",
        line(&|column, width| paint(format!("{:<width$}", HEADERS[column]), Style::Bold))
    );
    for row in rows {
        println!(
            "{}",
            line(&|column, width| match column {
                NAME_COLUMN => paint(format!("{:<width$}", row[column]), Style::Name),
                _ => format!("{:<width$}", row[column]),
            })
        );
    }
}
