use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::diff::{ChangeKind, FileChange, OldContent};
use crate::error::{ForkspaceError, Result};
use crate::filter::{ChangeClass, ChangeFilter};
use crate::manifest::{self, ManifestKind, ManifestRow, ManifestStatus};
use crate::merge_plan;
use crate::util::{forkspace_home, now_rfc3339, now_unix_ns, IoContext, WalkDirExt};
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PullPhase {
    Planned,
    Applying,
    Applied,
    Rebaselining,
    Done,
}

#[derive(Debug, Serialize, Deserialize)]
struct PullJournal {
    op: String,
    workspace: String,
    source: PathBuf,
    target: PathBuf,
    changes: Vec<FileChange>,
    next: usize,
    phase: PullPhase,
    fork_time_ns: u128,
}

fn timestamp_ns(metadata: &fs::Metadata) -> i128 {
    i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
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

fn matches_row(path: &Path, row: &ManifestRow) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    kind(&metadata) == row.kind
        && (row.kind == ManifestKind::Dir || metadata.len() == row.size)
        && (matches!(row.kind, ManifestKind::Dir | ManifestKind::Symlink)
            || timestamp_ns(&metadata) == row.mtime_ns)
        && (row.mode == 0 || metadata.mode() & 0o7777 == row.mode)
        && (row.kind != ManifestKind::Symlink
            || fs::read_link(path)
                .ok()
                .is_some_and(|target| Some(manifest::encode_path(&target)) == row.target))
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
            Some(row) if !row.dirty && matches_row(entry.path(), row) => {}
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

fn journal_path(workspace: &Workspace) -> PathBuf {
    forkspace_home().join("journal").join(format!(
        "pull-{}-{}.json",
        workspace.name, workspace.fork_time_ns
    ))
}

fn save(path: &Path, journal: &PullJournal) -> Result {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).io_context(|| format!("create {}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(journal)
            .map_err(|error| ForkspaceError::Serialization(error.to_string()))?,
    )
    .io_context(|| format!("write {}", temporary.display()))?;
    fs::File::open(&temporary)
        .and_then(|file| file.sync_all())
        .io_context(|| format!("sync {}", temporary.display()))?;
    fs::rename(&temporary, path).io_context(|| format!("install {}", path.display()))?;
    Ok(())
}

fn load(path: &Path) -> Result<PullJournal> {
    serde_json::from_slice(&fs::read(path).io_context(|| format!("read {}", path.display()))?)
        .map_err(|error| ForkspaceError::StateCorrupted(format!("invalid pull journal: {error}")))
}

fn apply_remaining(path: &Path, journal: &mut PullJournal) -> Result {
    journal.phase = PullPhase::Applying;
    save(path, journal)?;
    while journal.next < journal.changes.len() {
        merge_plan::apply_change(
            &journal.source,
            &journal.target,
            &journal.changes[journal.next],
            journal.next,
            "pull",
        )?;
        journal.next += 1;
        save(path, journal)?;
    }
    journal.phase = PullPhase::Applied;
    save(path, journal)
}

pub fn start(workspace: &Workspace, plan: &PullPlan) -> Result<PathBuf> {
    let path = journal_path(workspace);
    let mut journal = PullJournal {
        op: "pull".to_string(),
        workspace: workspace.name.clone(),
        source: plan.source.clone(),
        target: plan.target.clone(),
        changes: plan.changes.clone(),
        next: 0,
        phase: PullPhase::Planned,
        fork_time_ns: workspace.fork_time_ns,
    };
    save(&path, &journal)?;
    apply_remaining(&path, &mut journal)?;
    Ok(path)
}

pub fn resume_apply(workspace: &Workspace) -> Result<PathBuf> {
    let path = find_open_journal(&workspace.name)?.ok_or_else(|| {
        ForkspaceError::InvalidArgument(format!(
            "no interrupted pull journal for '{}'",
            workspace.name
        ))
    })?;
    let mut journal = load(&path)?;
    if matches!(journal.phase, PullPhase::Planned | PullPhase::Applying) {
        apply_remaining(&path, &mut journal)?;
    }
    Ok(path)
}

pub fn finish(workspace: &mut Workspace, path: &Path) -> Result {
    let mut journal = load(path)?;
    journal.phase = PullPhase::Rebaselining;
    save(path, &journal)?;
    let fork_time_ns = now_unix_ns();
    // The new ancestor is the current origin, not the possibly still-edited workspace.
    // This preserves ours-only edits in subsequent diff and merge results.
    manifest::build(&workspace.name, &workspace.base, fork_time_ns)?;
    workspace.fork_time_ns = fork_time_ns;
    workspace.manifest_generation = workspace.manifest_generation.saturating_add(1);
    workspace.last_pull = Some(now_rfc3339());
    workspace.manifest_status = ManifestStatus::Ready;
    save(path, &journal)
}

pub fn complete(path: &Path) -> Result {
    let mut journal = load(path)?;
    journal.phase = PullPhase::Done;
    save(path, &journal)?;
    fs::remove_file(path).io_context(|| format!("remove {}", path.display()))
}

pub fn find_open_journal(workspace: &str) -> Result<Option<PathBuf>> {
    let directory = forkspace_home().join("journal");
    let Ok(entries) = fs::read_dir(&directory) else {
        return Ok(None);
    };
    let prefix = format!("pull-{workspace}-");
    for entry in entries {
        let path = entry
            .io_context(|| format!("read {}", directory.display()))?
            .path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".json"))
        {
            let journal = load(&path)?;
            if journal.phase != PullPhase::Done {
                return Ok(Some(path));
            }
            let _ = fs::remove_file(&path);
        }
    }
    Ok(None)
}

pub fn guard(workspace: &str) -> Result {
    if find_open_journal(workspace)?.is_some() {
        return Err(ForkspaceError::StaleAncestor(workspace.to_string()));
    }
    Ok(())
}
