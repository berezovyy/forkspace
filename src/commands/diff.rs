//! `forkspace diff`: what changed in a workspace, as a short list and then a Git patch that
//! `git apply` takes; with several workspaces, how their attempts compare.

use std::io::Write;

use crate::backend::select_backend;
use crate::changes::manifest::ManifestStatus;
use crate::changes::{format_diff, DiffFormat, DiffOutput};
use crate::cli::DiffArgs;
use crate::error::Result;
use crate::review;
use crate::state::State;
use crate::util::IoContext;
use crate::workspace::{guard, Workspace};

pub fn exec(args: DiffArgs) -> Result {
    if args.names.len() > 1 {
        return compare(&args.names, args.json);
    }
    let name = crate::workspace::pick(args.names.first().cloned())?;
    guard::ensure_idle(&name)?;
    if let Err(error) = crate::changes::manifest::settle_now(&name, false) {
        eprintln!("warning: {error}");
    }
    let state = State::load()?;
    let workspace = state.require_workspace(&name)?;
    guard::require_view(workspace)?;

    let backend = select_backend(Some(workspace.backend_kind))?;
    let changes = backend.changes(workspace, false, true)?;
    let signals = review::signals::collect(workspace, &changes);
    warn_about_manifest(manifest_status_after_scan(workspace)?);

    if args.json {
        let output = DiffOutput {
            workspace: name.clone(),
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
    println!("{}", format_diff(&changes, &DiffFormat::Default));
    if let Some(line) = review::signals::render(&signals) {
        println!("{line}");
    }
    let (patch, omitted) =
        crate::changes::patch::partial_patch(workspace, &backend.view_path(workspace), &changes);
    if !patch.is_empty() {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(b"\n")
            .and_then(|()| stdout.write_all(&patch))
            .and_then(|()| stdout.flush())
            .io_context(|| "write the patch".to_string())?;
    }
    for omission in omitted {
        eprintln!(
            "note: {} is not in the patch: {}",
            omission.path.display(),
            omission.reason
        );
    }
    Ok(())
}

/// `diff a b ...`: side-by-side facts about sibling workspaces of one project.
fn compare(names: &[String], json: bool) -> Result {
    for name in names {
        if let Err(error) = crate::changes::manifest::settle_now(name, false) {
            eprintln!("warning: {error}");
        }
    }
    let state = State::load()?;
    let workspaces = names
        .iter()
        .map(|name| state.require_workspace(name).cloned())
        .collect::<Result<Vec<_>>>()?;
    let output = review::compare::build(&workspaces)?;
    if json {
        super::print_json(&output)
    } else {
        println!("{}", review::compare::render(&output));
        Ok(())
    }
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
        ManifestStatus::Building => eprintln!(
            "warning: deleted files are not listed yet: forkspace is still listing the files the workspace started with"
        ),
        ManifestStatus::Degraded => {
            eprintln!("warning: deleted files cannot be listed: forkspace lost its list of the files the workspace started with");
        }
        ManifestStatus::Absent | ManifestStatus::Ready => {}
    }
}
