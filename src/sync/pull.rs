use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

use crate::changes::filter::{ChangeClass, ChangeFilter};
use crate::changes::manifest::{self, ManifestKind, ManifestStatus};
use crate::changes::{ChangeKind, FileChange, OldContent};
use crate::error::{ForkspaceError, Result};
use crate::sync::journal::{self, OperationKind, OperationPhase};
use crate::util::{now_rfc3339, now_unix_ns, WalkDirExt};
use crate::workspace::Workspace;

#[derive(Debug, Clone, Serialize)]
pub struct PullPlan {
    pub workspace: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub changes: Vec<FileChange>,
    pub conflicts: Vec<PathBuf>,
    pub convergent: Vec<PathBuf>,
}

fn kind(metadata: &fs::Metadata) -> ManifestKind {
    if metadata.file_type().is_symlink() {
        ManifestKind::Symlink
    } else if metadata.is_dir() {
        ManifestKind::Dir
    } else {
        ManifestKind::File
    }
}

fn equivalent(left: &Path, right: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::symlink_metadata(left), fs::symlink_metadata(right)) else {
        return fs::symlink_metadata(left).is_err() && fs::symlink_metadata(right).is_err();
    };
    if kind(&a) != kind(&b) || a.mode() & 0o7777 != b.mode() & 0o7777 {
        return false;
    }
    if a.file_type().is_symlink() {
        return fs::read_link(left).ok() == fs::read_link(right).ok();
    }
    if a.is_dir() {
        return true;
    }
    if a.len() != b.len() {
        return false;
    }
    let (Ok(mut a), Ok(mut b)) = (fs::File::open(left), fs::File::open(right)) else {
        return false;
    };
    let mut ab = [0; 8192];
    let mut bb = [0; 8192];
    loop {
        let (Ok(an), Ok(bn)) = (a.read(&mut ab), b.read(&mut bb)) else {
            return false;
        };
        if an != bn || ab[..an] != bb[..bn] {
            return false;
        }
        if an == 0 {
            return true;
        }
    }
}

fn file_change(path: PathBuf, change: ChangeKind) -> FileChange {
    FileChange {
        class: ChangeFilter::class(&path),
        path,
        kind: change,
        additions: None,
        deletions: None,
        is_binary: false,
        old_content: OldContent::Available,
    }
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
    let mut current = BTreeSet::new();
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
        current.insert(relative.clone());
        let class = ChangeFilter::class(&relative);
        if matches!(class, ChangeClass::Runtime | ChangeClass::Internal) {
            continue;
        }
        match rows.get(&relative) {
            Some(row) if !row.dirty && crate::sync::plan::matches_ancestor(entry.path(), row) => {}
            Some(_) => changes.push(file_change(relative, ChangeKind::Modified)),
            None => changes.push(file_change(relative, ChangeKind::Added)),
        }
    }
    for (path, row) in rows {
        if !current.contains(&path)
            && !matches!(
                ChangeFilter::class(&path),
                ChangeClass::Runtime | ChangeClass::Internal
            )
        {
            let _ = row;
            changes.push(file_change(path, ChangeKind::Deleted));
        }
    }
    changes.sort_by(|a, b| {
        a.path
            .as_os_str()
            .as_encoded_bytes()
            .cmp(b.path.as_os_str().as_encoded_bytes())
    });
    Ok(changes)
}

pub fn build(workspace: &Workspace, theirs: bool) -> Result<PullPlan> {
    let backend = crate::backend::select_backend(Some(workspace.backend_kind))?;
    let theirs_delta = backend.base_changes(workspace)?;
    let ours_delta = backend.changes(workspace, true, false)?;
    build_from_changes(workspace, theirs_delta, ours_delta, theirs)
}

pub fn build_from_changes(
    workspace: &Workspace,
    theirs_delta: Vec<FileChange>,
    ours_delta: Vec<FileChange>,
    theirs: bool,
) -> Result<PullPlan> {
    let ours: BTreeSet<_> = ours_delta
        .into_iter()
        .filter(|change| !matches!(change.class, ChangeClass::Runtime | ChangeClass::Internal))
        .map(|change| change.path)
        .collect();
    let mut changes = Vec::new();
    let mut conflicts = Vec::new();
    let mut convergent = Vec::new();
    for change in theirs_delta {
        if ours.contains(&change.path) {
            if equivalent(
                &workspace.base.join(&change.path),
                &workspace.view_path.join(&change.path),
            ) {
                convergent.push(change.path);
            } else if theirs {
                conflicts.push(change.path.clone());
                changes.push(change);
            } else {
                conflicts.push(change.path);
            }
        } else {
            changes.push(change);
        }
    }
    conflicts.sort();
    convergent.sort();
    if !conflicts.is_empty() && !theirs {
        return Err(ForkspaceError::PullConflict {
            paths: conflicts
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
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

pub fn render(plan: &PullPlan) -> String {
    let mut lines = vec![format!(
        "pull plan for '{}': {} files",
        plan.workspace,
        plan.changes.len()
    )];
    for change in &plan.changes {
        let action = match change.kind {
            ChangeKind::Added => "add",
            ChangeKind::Modified => "update",
            ChangeKind::Deleted => "delete",
        };
        lines.push(format!("  {action} {}", change.path.display()));
    }
    if !plan.conflicts.is_empty() {
        lines.push(format!(
            "taking base for conflicts: {}",
            plan.conflicts
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.join("\n")
}

pub fn start(workspace: &Workspace, plan: &PullPlan) -> Result<PathBuf> {
    journal::start(
        OperationKind::Pull,
        &workspace.name,
        plan.source.clone(),
        plan.target.clone(),
        plan.changes.clone(),
        workspace.fork_time_ns,
    )
}

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

pub fn finish(workspace: &mut Workspace, path: &Path) -> Result {
    journal::set_phase(path, OperationPhase::Rebaselining)?;
    let fork_time_ns = now_unix_ns();
    // The new ancestor is the current origin, not the possibly still-edited workspace.
    // This preserves ours-only edits in subsequent diff and merge results.
    manifest::build(&workspace.name, &workspace.base, fork_time_ns)?;
    workspace.fork_time_ns = fork_time_ns;
    workspace.manifest_generation = workspace.manifest_generation.saturating_add(1);
    workspace.last_pull = Some(now_rfc3339());
    workspace.manifest_status = ManifestStatus::Ready;
    Ok(())
}

pub fn complete(path: &Path) -> Result {
    journal::set_phase(path, OperationPhase::Done)?;
    journal::remove(path)
}

pub fn find_open_journal(workspace: &str) -> Result<Option<PathBuf>> {
    journal::find_open(OperationKind::Pull, workspace)
}
