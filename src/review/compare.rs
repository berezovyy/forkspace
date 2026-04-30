//! `compare`: per-workspace change counts and signals for sibling attempts on one base, and
//! which paths they contest, share partially, or touch alone.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use serde::Serialize;

use crate::backend::select_backend;
use crate::changes::filter::ChangeClass;
use crate::changes::manifest::ManifestStatus;
use crate::changes::ChangeCounts;
use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::review::signals::{self, Signals};
use crate::sync::join_paths;
use crate::workspace::{guard, Workspace};

/// One workspace's source-change summary.
#[derive(Debug, Clone, Serialize)]
pub struct CompareRow {
    pub workspace: String,
    /// The fork manifest is still building or degraded, so no changes were collected.
    pub pending: bool,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub dependencies: Signals,
    pub tests: usize,
    pub changes: BTreeMap<PathBuf, ChangeKind>,
}

/// The comparison of two or more workspaces forked from the same base.
#[derive(Debug, Clone, Serialize)]
pub struct CompareOutput {
    pub base: PathBuf,
    /// Rows in argument order.
    pub workspaces: Vec<CompareRow>,
    /// Paths changed by every non-pending workspace.
    pub contested: Vec<PathBuf>,
    /// Per workspace, the paths only it changed.
    pub unique: Vec<(String, Vec<PathBuf>)>,
    /// Paths changed by more than one workspace but not all of them.
    pub partial: Vec<PathBuf>,
    /// Pending workspaces, excluded from divergence.
    pub degraded: Vec<String>,
}

/// Compare `workspaces`, which must be at least two distinct siblings of one base.
pub fn build(workspaces: &[Workspace]) -> Result<CompareOutput> {
    let base = validate(workspaces)?;
    let mut rows = Vec::with_capacity(workspaces.len());
    let mut degraded = Vec::new();
    for workspace in workspaces {
        guard::ensure_no_interrupted_pull(&workspace.name)?;
        if matches!(
            workspace.manifest_status,
            ManifestStatus::Building | ManifestStatus::Degraded
        ) {
            degraded.push(workspace.name.clone());
            rows.push(CompareRow::pending(&workspace.name));
        } else {
            rows.push(CompareRow::collect(workspace)?);
        }
    }

    let Divergence {
        contested,
        unique,
        partial,
    } = divergence(&rows);
    Ok(CompareOutput {
        base,
        workspaces: rows,
        contested,
        unique,
        partial,
        degraded,
    })
}

/// The summary table followed by divergence notes.
pub fn render(output: &CompareOutput) -> String {
    let mut lines = vec![
        format!(
            "BASE  {}   {} workspaces, same base",
            output.base.display(),
            output.workspaces.len()
        ),
        String::new(),
        "WORKSPACE  SOURCE          DEPS  TESTS".to_string(),
    ];
    lines.extend(output.workspaces.iter().map(table_row));
    if !output.contested.is_empty() {
        lines.push(format!(
            "contested (all {}): {}",
            output.workspaces.len() - output.degraded.len(),
            join_paths(&output.contested)
        ));
    }
    for (workspace, paths) in &output.unique {
        if !paths.is_empty() {
            lines.push(format!("unique to {workspace}: {}", join_paths(paths)));
        }
    }
    if !output.partial.is_empty() {
        lines.push(format!(
            "{} paths touched by some but not all — use --files",
            output.partial.len()
        ));
    }
    if !output.degraded.is_empty() {
        lines.push(format!(
            "degraded: {} pending manifest; excluded from divergence",
            output.degraded.join(", ")
        ));
    }
    lines.join("\n")
}

/// A path-by-workspace matrix of `A`/`M`/`D`/`-`.
pub fn render_files(output: &CompareOutput) -> String {
    let names = output
        .workspaces
        .iter()
        .map(|row| row.workspace.as_str())
        .collect::<Vec<_>>();
    let mut lines = vec![format!("PATH  {}", names.join("  "))];
    for path in changed_paths(&output.workspaces) {
        let cells = output
            .workspaces
            .iter()
            .map(|row| match row.changes.get(&path) {
                Some(ChangeKind::Added) => "A",
                Some(ChangeKind::Modified) => "M",
                Some(ChangeKind::Deleted) => "D",
                None => "-",
            })
            .collect::<Vec<_>>();
        lines.push(format!("{}  {}", path.display(), cells.join("  ")));
    }
    lines.join("\n")
}

impl CompareRow {
    fn pending(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
            pending: true,
            added: 0,
            modified: 0,
            deleted: 0,
            dependencies: Signals::default(),
            tests: 0,
            changes: BTreeMap::new(),
        }
    }

    /// Counts cover source changes only; signals see every change, including lockfiles.
    fn collect(workspace: &Workspace) -> Result<Self> {
        let changes =
            select_backend(Some(workspace.backend_kind))?.changes(workspace, false, false)?;
        let source: Vec<&FileChange> = changes
            .iter()
            .filter(|change| change.class == ChangeClass::Source)
            .collect();
        let counts = ChangeCounts::of(source.iter().copied());
        let signals = signals::collect(workspace, &changes);
        Ok(Self {
            workspace: workspace.name.clone(),
            pending: false,
            added: counts.added,
            modified: counts.modified,
            deleted: counts.deleted,
            tests: signals.tests_touched,
            dependencies: signals,
            changes: source
                .into_iter()
                .map(|change| (change.path.clone(), change.kind))
                .collect(),
        })
    }
}

/// Reject fewer than two workspaces, duplicates, and workspaces that do not share the first
/// one's base and parent. Returns the shared (canonical) base.
fn validate(workspaces: &[Workspace]) -> Result<PathBuf> {
    let [first, rest @ ..] = workspaces else {
        return Err(too_few());
    };
    if rest.is_empty() {
        return Err(too_few());
    }
    let mut seen = HashSet::new();
    if let Some(duplicate) = workspaces
        .iter()
        .find(|workspace| !seen.insert(&workspace.name))
    {
        return Err(ForkspaceError::InvalidArgument(format!(
            "duplicate workspace '{}' in comparison",
            duplicate.name
        )));
    }
    let expected = first.compare_key();
    if let Some(stranger) = rest
        .iter()
        .find(|workspace| workspace.compare_key() != expected)
    {
        return Err(ForkspaceError::BaseMismatch {
            workspace: stranger.name.clone(),
            base: stranger.base.display().to_string(),
            expected: first.base.display().to_string(),
        });
    }
    Ok(expected.0)
}

fn too_few() -> ForkspaceError {
    ForkspaceError::InvalidArgument("compare requires at least two workspaces".to_string())
}

/// Changed paths bucketed by how many workspaces touched them; see [`CompareOutput`].
struct Divergence {
    contested: Vec<PathBuf>,
    unique: Vec<(String, Vec<PathBuf>)>,
    partial: Vec<PathBuf>,
}

fn divergence(rows: &[CompareRow]) -> Divergence {
    let active = rows.iter().filter(|row| !row.pending).count();
    let mut contested = Vec::new();
    let mut partial = Vec::new();
    let mut unique: Vec<_> = rows
        .iter()
        .map(|row| (row.workspace.clone(), Vec::new()))
        .collect();
    for path in changed_paths(rows) {
        let owners: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.changes.contains_key(&path))
            .map(|(index, _)| index)
            .collect();
        match owners.as_slice() {
            _ if active > 0 && owners.len() == active => contested.push(path),
            [only] => unique[*only].1.push(path),
            [_, _, ..] => partial.push(path),
            [] => {}
        }
    }
    Divergence {
        contested,
        unique,
        partial,
    }
}

/// Every path any row changed, sorted.
fn changed_paths(rows: &[CompareRow]) -> BTreeSet<PathBuf> {
    rows.iter()
        .flat_map(|row| row.changes.keys().cloned())
        .collect()
}

fn table_row(row: &CompareRow) -> String {
    if row.pending {
        return format!("{:<10} -               -     -", row.workspace);
    }
    let net_deps = row.dependencies.deps_added as isize - row.dependencies.deps_removed as isize;
    format!(
        "{:<10} {}+ {}~ {}-   {net_deps:<+5} {}",
        row.workspace, row.added, row.modified, row.deleted, row.tests
    )
}
