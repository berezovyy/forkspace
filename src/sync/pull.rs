//! Pull: bring base-side changes made since the fork into a workspace, then re-baseline.
//!
//! The lifecycle is [`build`] → [`start`] (journaled apply) → [`finish`] (new fork-time
//! manifest) → [`complete`] (journal removed). [`resume_apply`] picks up an interrupted apply.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

use crate::backend::overlay::frozen;
use crate::backend::{select_backend, BackendState};
use crate::changes::filter::ChangeFilter;
use crate::changes::manifest::{self, ManifestKind, ManifestStatus};
use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::sync::journal::{self, OperationKind, OperationPhase};
use crate::sync::plan::matches_ancestor;
use crate::sync::{display_paths, is_transferable, render_plan};
use crate::util::fs::{mode_bits, same_contents};
use crate::util::{now_rfc3339, now_unix_ns, WalkDirExt};
use crate::workspace::Workspace;

/// The base-side changes a pull would copy into a workspace.
#[derive(Debug, Clone, Serialize)]
pub struct PullPlan {
    pub workspace: String,
    /// The base directory changes are copied from.
    pub source: PathBuf,
    /// The workspace view changes land in.
    pub target: PathBuf,
    pub changes: Vec<FileChange>,
    /// Paths both sides changed differently.
    pub conflicts: Vec<PathBuf>,
    /// Paths both sides changed to the same result; nothing to copy.
    pub convergent: Vec<PathBuf>,
}

/// Compute the base-side delta against the fork-time manifest.
pub fn base_changes(workspace: &Workspace) -> Result<Vec<FileChange>> {
    if workspace.manifest_status != ManifestStatus::Ready
        || !manifest::manifest_path(&workspace.name).exists()
    {
        return Err(ForkspaceError::InvalidArgument(format!(
            "pull unavailable: workspace '{}' has no ready fork-time manifest; merge or preserve its changes, then re-fork",
            workspace.name
        )));
    }
    if !workspace.base.is_dir() {
        return Err(ForkspaceError::InvalidArgument(format!(
            "base directory for '{}' is missing; restore it before pulling",
            workspace.name
        )));
    }
    let mut rows = BTreeMap::new();
    for row in manifest::stream(&workspace.name)? {
        let row = row?;
        rows.insert(manifest::decode_path(&row.path)?, row);
    }

    let mut present = BTreeSet::new();
    let mut changes = Vec::new();
    for entry in WalkDir::new(&workspace.base)
        .min_depth(1)
        .follow_links(false)
    {
        let entry = entry.walk_context(&workspace.base)?;
        let relative = entry
            .path()
            .strip_prefix(&workspace.base)
            .map_err(|_| {
                ForkspaceError::StateFailed("base scan escaped the workspace origin".to_string())
            })?
            .to_path_buf();
        present.insert(relative.clone());
        if !is_transferable(ChangeFilter::class(&relative)) {
            continue;
        }
        match rows.get(&relative) {
            Some(row) if !row.dirty && matches_ancestor(entry.path(), row) => {}
            Some(_) => changes.push(FileChange::new(relative, ChangeKind::Modified)),
            None => changes.push(FileChange::new(relative, ChangeKind::Added)),
        }
    }
    changes.extend(
        rows.into_keys()
            .filter(|path| !present.contains(path) && is_transferable(ChangeFilter::class(path)))
            .map(|path| FileChange::new(path, ChangeKind::Deleted)),
    );
    // Raw byte order rather than `Path`'s component-wise order.
    changes.sort_by(|a, b| {
        a.path
            .as_os_str()
            .as_encoded_bytes()
            .cmp(b.path.as_os_str().as_encoded_bytes())
    });
    Ok(changes)
}

/// Plan a pull into `workspace`. With `theirs`, conflicting paths take the base version;
/// without it, any conflict is an error.
pub fn build(workspace: &Workspace, theirs: bool) -> Result<PullPlan> {
    let backend = select_backend(Some(workspace.backend_kind))?;
    let theirs_delta = backend.base_changes(workspace)?;
    let ours_delta = backend.changes(workspace, true, false)?;
    crate::util::trace::time("plan", || {
        build_from_changes(workspace, theirs_delta, ours_delta, theirs)
    })
}

/// Plan a pull from already-collected base-side (`theirs_delta`) and workspace-side
/// (`ours_delta`) changes.
pub fn build_from_changes(
    workspace: &Workspace,
    theirs_delta: Vec<FileChange>,
    ours_delta: Vec<FileChange>,
    theirs: bool,
) -> Result<PullPlan> {
    let ours: BTreeSet<_> = ours_delta
        .into_iter()
        .filter(|change| is_transferable(change.class))
        .map(|change| change.path)
        .collect();
    let mut changes = Vec::new();
    let mut conflicts = Vec::new();
    let mut convergent = Vec::new();
    for change in theirs_delta {
        if !ours.contains(&change.path) {
            changes.push(change);
        } else if equivalent(
            &workspace.base.join(&change.path),
            &workspace.view_path.join(&change.path),
        ) {
            convergent.push(change.path);
        } else {
            conflicts.push(change.path.clone());
            if theirs {
                changes.push(change);
            }
        }
    }
    conflicts.sort();
    convergent.sort();
    if !conflicts.is_empty() && !theirs {
        return Err(ForkspaceError::PullConflict {
            paths: display_paths(&conflicts),
        });
    }
    Ok(PullPlan {
        workspace: workspace.name.clone(),
        source: workspace.base.clone(),
        target: workspace.view_path.clone(),
        changes,
        conflicts,
        convergent,
    })
}

/// Human-readable plan, one line per change plus the conflicts resolved toward the base.
pub fn render(plan: &PullPlan) -> String {
    render_plan(
        format!(
            "pull plan for '{}': {} files",
            plan.workspace,
            plan.changes.len()
        ),
        &plan.changes,
        |kind| match kind {
            ChangeKind::Added => "add",
            ChangeKind::Modified => "update",
            ChangeKind::Deleted => "delete",
        },
        &[("taking base for conflicts", &plan.conflicts)],
    )
}

/// Journal and apply `plan`, returning the journal path for [`finish`] and [`complete`].
pub fn start(workspace: &Workspace, plan: &PullPlan) -> Result<PathBuf> {
    let _apply = crate::util::trace::span("apply");
    journal::start(
        OperationKind::Pull,
        &workspace.name,
        plan.source.clone(),
        plan.target.clone(),
        plan.changes.clone(),
        workspace.fork_time_ns,
    )
}

/// Finish applying the interrupted pull of `workspace`, if its steps are not all applied yet.
pub fn resume_apply(workspace: &Workspace) -> Result<PathBuf> {
    let path = find_open_journal(&workspace.name)?.ok_or_else(|| {
        ForkspaceError::InvalidArgument(format!(
            "no interrupted pull journal for '{}'",
            workspace.name
        ))
    })?;
    let journal = journal::load(&path)?;
    if journal.op != OperationKind::Pull {
        return Err(ForkspaceError::StateCorrupted(format!(
            "{} is not a pull journal",
            path.display()
        )));
    }
    if matches!(
        journal.phase,
        OperationPhase::Planned | OperationPhase::Applying
    ) {
        journal::apply_remaining(&path)?;
    }
    Ok(path)
}

/// Re-baseline `workspace` on the current base. The caller must persist state before
/// [`complete`] removes the journal.
pub fn finish(workspace: &mut Workspace, path: &Path) -> Result {
    journal::set_phase(path, OperationPhase::Rebaselining)?;
    // The new ancestor is the current origin, not the possibly still-edited workspace.
    // This preserves ours-only edits in subsequent diff and merge results. A sealed overlay
    // measures its changes against a frozen copy, so it gets a fresh one of the origin; the
    // view keeps its mounted lower layer, and the pulled files in its upper layer now match
    // the ancestor.
    let ancestor = match &mut workspace.backend_state {
        BackendState::Overlay(layers) if layers.lower.is_some() => {
            let frozen = frozen::freeze(&workspace.base, frozen::Basis::Project)?;
            layers.ancestor =
                (layers.lower.as_ref() != Some(&frozen.tree)).then(|| frozen.tree.clone());
            frozen.tree
        }
        _ => workspace.base.clone(),
    };
    let fork_time_ns = now_unix_ns();
    manifest::build(&workspace.name, &ancestor, fork_time_ns)?;
    workspace.fork_time_ns = fork_time_ns;
    workspace.manifest_generation = workspace.manifest_generation.saturating_add(1);
    workspace.last_pull = Some(now_rfc3339());
    workspace.manifest_status = ManifestStatus::Ready;
    Ok(())
}

/// Mark the pull journal at `path` done and remove it.
pub fn complete(path: &Path) -> Result {
    journal::set_phase(path, OperationPhase::Done)?;
    journal::remove(path)
}

/// The interrupted pull journal of `workspace`, if any.
pub fn find_open_journal(workspace: &str) -> Result<Option<PathBuf>> {
    journal::find_open(OperationKind::Pull, workspace)
}

/// Whether both entries have the same kind, permissions, and content (or are both missing).
fn equivalent(left: &Path, right: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::symlink_metadata(left), fs::symlink_metadata(right)) else {
        return fs::symlink_metadata(left).is_err() && fs::symlink_metadata(right).is_err();
    };
    if ManifestKind::of(&a) != ManifestKind::of(&b) || mode_bits(&a) != mode_bits(&b) {
        return false;
    }
    if a.file_type().is_symlink() {
        return fs::read_link(left).ok() == fs::read_link(right).ok();
    }
    a.is_dir() || (a.len() == b.len() && same_contents(left, right))
}
