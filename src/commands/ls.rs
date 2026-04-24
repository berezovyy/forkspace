use std::path::Path;

use owo_colors::OwoColorize;

use crate::backend::{select_backend, ChangeCount};
use crate::error::Result;
use crate::state::State;

fn shorten_path(path: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = path.strip_prefix(&cwd) {
            let rel_str = rel.display().to_string();
            if rel_str.is_empty() {
                return ".".to_string();
            }
            return format!("./{}", rel_str);
        }
    }
    path.display().to_string()
}

pub fn exec(args: crate::cli::LsArgs) -> Result {
    let state = State::load()?;

    if args.names {
        let mut names: Vec<_> = state.workspaces.keys().collect();
        names.sort();
        for name in names {
            println!("{}", name);
        }
        return Ok(());
    }

    let mut workspaces: Vec<_> = state.workspaces.values().collect();
    workspaces.sort_by(|a, b| a.name.cmp(&b.name));

    if args.json {
        let values: Vec<_> = workspaces
            .iter()
            .map(|workspace| {
                serde_json::json!({
                    "name": workspace.name,
                    "base": workspace.base,
                    "parent": workspace.parent,
                    "branch": workspace.branch,
                    "backend": workspace.backend_kind.as_str(),
                    "degraded": workspace.degraded,
                    "probe_reason": workspace.probe_reason,
                    "base_missing": crate::reconcile::base_missing(&workspace.base),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&values)
                .map_err(|error| crate::error::ForkspaceError::Serialization(error.to_string()))?
        );
        return Ok(());
    }

    if workspaces.is_empty() {
        println!("no workspaces");
        return Ok(());
    }

    let show_branch = workspaces
        .iter()
        .any(|workspace| workspace.branch.is_some());
    let rows: Vec<(String, String, String, String, String, String)> = workspaces
        .iter()
        .map(|ws| -> Result<_> {
            let mut origin = if let Some(ref parent) = ws.parent {
                parent.clone()
            } else {
                shorten_path(&ws.base)
            };
            if crate::reconcile::base_missing(&ws.base) {
                origin.push_str(" (missing)");
            }

            let changed =
                match select_backend(Some(ws.backend_kind))?.change_count(ws, args.no_gitignore)? {
                    ChangeCount::Ready(1) => "1 file".to_string(),
                    ChangeCount::Ready(count) => format!("{count} files"),
                    ChangeCount::Pending => "-".to_string(),
                    ChangeCount::Degraded => "degraded".to_string(),
                };

            let proc_info = match &ws.process {
                Some(p) if crate::process_watch::is_alive(p.pid, p.identity.as_deref()) => {
                    format!(":{} ({})", p.port, p.command)
                }
                Some(_) => "stopped".to_string(),
                None => "-".to_string(),
            };

            Ok((
                ws.name.clone(),
                origin,
                ws.branch.clone().unwrap_or_else(|| "-".to_string()),
                ws.backend_kind.as_str().to_string(),
                changed,
                proc_info,
            ))
        })
        .collect::<Result<_>>()?;

    let w_name = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(4);
    let w_base = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(4);
    let w_branch = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(6);
    let w_backend = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(7);
    let w_changed = rows.iter().map(|r| r.4.len()).max().unwrap_or(0).max(7);

    if show_branch {
        println!(
            "{:<w_name$}  {:<w_base$}  {:<w_branch$}  {:<w_backend$}  {:<w_changed$}  {}",
            "NAME".bold(),
            "BASE".bold(),
            "BRANCH".bold(),
            "BACKEND".bold(),
            "CHANGED".bold(),
            "PROCESS".bold(),
        );
    } else {
        println!(
            "{:<w_name$}  {:<w_base$}  {:<w_backend$}  {:<w_changed$}  {}",
            "NAME".bold(),
            "BASE".bold(),
            "BACKEND".bold(),
            "CHANGED".bold(),
            "PROCESS".bold(),
        );
    }

    for (name, base, branch, backend, changed, proc_info) in &rows {
        if show_branch {
            println!(
                "{:<w_name$}  {:<w_base$}  {:<w_branch$}  {:<w_backend$}  {:<w_changed$}  {}",
                name.bold().bright_white(),
                base,
                branch,
                backend,
                changed,
                proc_info,
            );
        } else {
            println!(
                "{:<w_name$}  {:<w_base$}  {:<w_backend$}  {:<w_changed$}  {}",
                name.bold().bright_white(),
                base,
                backend,
                changed,
                proc_info,
            );
        }
    }

    Ok(())
}
