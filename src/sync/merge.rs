//! Applying a [`MergePlan`] to the base, resuming an interrupted merge, and the merge-adjacent
//! outputs (unified patch, Git commit).

use std::path::{Path, PathBuf};

use crate::backend::select_backend;
use crate::changes::manifest::ManifestStatus;
use crate::changes::ChangeCounts;
use crate::changes::{self, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::sync::journal::{self, OperationKind};
use crate::sync::plan::{self, MergePlan};
use crate::workspace::Workspace;

/// What a completed [`merge_workspace`] transferred.
pub struct MergeResult {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    /// Changes left out because they are ignored, runtime, or internal.
    pub skipped: usize,
}

/// Plan and apply a conflict-free merge of `workspace` into its base in one step.
pub fn merge_workspace(workspace: &Workspace) -> Result<MergeResult> {
    let plan = plan::build(workspace, false)?;
    let counts = ChangeCounts::of(&plan.changes);
    if !plan.changes.is_empty() {
        if workspace.manifest_status == ManifestStatus::Absent && plan.merged_steps.is_empty() {
            apply_without_journal(&plan)?;
        } else {
            let journal = execute(&plan)?;
            let held = journal::held(&journal)?;
            journal::finish_undo(&journal)?;
            journal::remove(&journal)?;
            if !held.is_empty() {
                return Err(held_error(&held));
            }
        }
    }
    Ok(MergeResult {
        added: counts.added,
        modified: counts.modified,
        deleted: counts.deleted,
        skipped: plan.skipped,
    })
}

/// Apply `plan` under a merge journal and return the journal's path.
///
/// The journal is left in the `applied` phase; the caller finishes any post-merge steps and
/// then removes it, so a crash in between is still visible as an interrupted merge.
pub fn execute(plan: &MergePlan) -> Result<PathBuf> {
    journal::start(
        OperationKind::Merge,
        &plan.workspace,
        plan.source.clone(),
        plan.target.clone(),
        plan.changes.clone(),
        plan.fork_time_ns,
        plan.merged_steps.clone(),
        Some(plan.guard.clone()),
    )
}

/// The error for a merge that left `held` paths alone because the base changed them while
/// it ran.
pub fn held_error(held: &[PathBuf]) -> ForkspaceError {
    ForkspaceError::ChangedDuring {
        operation: "merge".to_string(),
        paths: crate::sync::display_paths(held),
        remedy: "they changed while the merge ran (in your project, or in the workspace after it was read), so the workspace's versions were not applied there; the workspace was kept; review them and merge again".to_string(),
    }
}

/// Finish the interrupted merge journal of `workspace` and return its path.
pub fn resume(workspace: &Workspace) -> Result<PathBuf> {
    let path = journal::path(
        OperationKind::Merge,
        &workspace.name,
        workspace.fork_time_ns,
    );
    if !path.exists() {
        return Err(ForkspaceError::InvalidArgument(format!(
            "no interrupted merge journal for '{}'",
            workspace.name
        )));
    }
    if journal::load(&path)?.op != OperationKind::Merge {
        return Err(ForkspaceError::StateCorrupted(format!(
            "{} is not a merge journal",
            path.display()
        )));
    }
    journal::apply_remaining(&path)?;
    Ok(path)
}

/// Apply `plan` step by step without recording a journal (workspaces without a fork manifest).
fn apply_without_journal(plan: &MergePlan) -> Result {
    for (index, change) in plan.changes.iter().enumerate() {
        journal::apply_change(
            &plan.source,
            &plan.target,
            change,
            index,
            "merge",
            None,
            None,
        )?;
    }
    Ok(())
}

/// The Git patch that carries `changes` (new versions under `source`) into the base, or
/// [`ForkspaceError::PatchIncomplete`] naming every change it cannot carry. Never a partial
/// patch.
pub fn generate_patch(
    workspace: &Workspace,
    source: &Path,
    changes: &[FileChange],
) -> Result<Vec<u8>> {
    changes::patch::git_patch(workspace, source, changes).map_err(|omitted| {
        ForkspaceError::PatchIncomplete {
            workspace: workspace.name.clone(),
            omitted: omitted
                .into_iter()
                .map(|omission| format!("{} ({})", omission.path.display(), omission.reason))
                .collect(),
        }
    })
}

/// Whether the workspace moved its Git `HEAD`, i.e. made commits a file-level merge would flatten.
/// Workspaces recorded before the fork-time `HEAD` was kept fall back to a full change scan.
pub fn workspace_head_changed(workspace: &Workspace) -> Result<bool> {
    if let Some(head) = &workspace.git_head_at_fork {
        let current = crate::git::head_commit(&workspace.view_path).unwrap_or_default();
        return Ok(&current != head);
    }
    Ok(select_backend(Some(workspace.backend_kind))?
        .changes(workspace, true, false)?
        .iter()
        .any(|change| change.path == Path::new(".git/HEAD")))
}
