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
        if workspace.manifest_status == ManifestStatus::Absent {
            apply_without_journal(&plan)?;
        } else {
            journal::remove(&execute(&plan)?)?;
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
    let _apply = crate::util::trace::span("apply");
    journal::start(
        OperationKind::Merge,
        &plan.workspace,
        plan.source.clone(),
        plan.target.clone(),
        plan.changes.clone(),
        plan.fork_time_ns,
    )
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
        journal::apply_change(&plan.source, &plan.target, change, index, "merge")?;
    }
    Ok(())
}

/// Concatenated unified diffs for `changes`, each ending in a newline.
pub fn generate_patch(workspace: &Workspace, changes: &[FileChange]) -> Result<String> {
    let mut patch = String::new();
    for change in changes {
        let diff = changes::generate_unified_diff(workspace, change);
        patch.push_str(&diff);
        if !diff.ends_with('\n') {
            patch.push('\n');
        }
    }
    Ok(patch)
}

/// Whether the workspace moved its Git `HEAD`, i.e. made commits a file-level merge would flatten.
pub fn workspace_head_changed(workspace: &Workspace) -> Result<bool> {
    if let Some(head) = &workspace.git_head_at_fork {
        let current = crate::git::rev_parse(&workspace.view_path, "HEAD").unwrap_or_default();
        return Ok(&current != head);
    }
    Ok(select_backend(Some(workspace.backend_kind))?
        .changes(workspace, true, false)?
        .iter()
        .any(|change| change.path == Path::new(".git/HEAD")))
}

/// Stage everything in `base` and commit it with `message`.
pub fn git_commit(base: &Path, message: &str) -> Result {
    if !crate::git::is_repo(base) {
        return Err(ForkspaceError::GitFailed(
            "base directory is not a git repository".to_string(),
        ));
    }
    crate::git::run_git(base, &["add", "-A"])?;
    crate::git::run_git(base, &["commit", "-m", message])
}
