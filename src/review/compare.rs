use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use serde::Serialize;

use crate::backend::select_backend;
use crate::changes::filter::ChangeClass;
use crate::changes::manifest::ManifestStatus;
use crate::changes::{ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::review::signals::{self, Signals};
use crate::workspace::Workspace;

#[derive(Debug, Clone, Serialize)]
pub struct CompareRow {
    pub workspace: String,
    pub pending: bool,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub dependencies: Signals,
    pub tests: usize,
    pub changes: BTreeMap<PathBuf, ChangeKind>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompareOutput {
    pub base: PathBuf,
    pub workspaces: Vec<CompareRow>,
    pub contested: Vec<PathBuf>,
    pub unique: Vec<(String, Vec<PathBuf>)>,
    pub partial: Vec<PathBuf>,
    pub degraded: Vec<String>,
}

pub fn build(workspaces: &[Workspace]) -> Result<CompareOutput> {
    if workspaces.len() < 2 {
        return Err(ForkspaceError::InvalidArgument(
            "compare requires at least two workspaces".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for workspace in workspaces {
        if !seen.insert(&workspace.name) {
            return Err(ForkspaceError::InvalidArgument(format!(
                "duplicate workspace '{}' in comparison",
                workspace.name
            )));
        }
    }
    let expected = workspaces[0].compare_key();
    for workspace in &workspaces[1..] {
        if workspace.compare_key() != expected {
            return Err(ForkspaceError::BaseMismatch {
                workspace: workspace.name.clone(),
                base: workspace.base.display().to_string(),
                expected: workspaces[0].base.display().to_string(),
            });
        }
    }

    let mut rows = Vec::new();
    let mut degraded = Vec::new();
    for workspace in workspaces {
        crate::workspace::guard::ensure_no_interrupted_pull(&workspace.name)?;
        if matches!(
            workspace.manifest_status,
            ManifestStatus::Building | ManifestStatus::Degraded
        ) {
            degraded.push(workspace.name.clone());
            rows.push(CompareRow {
                workspace: workspace.name.clone(),
                pending: true,
                added: 0,
                modified: 0,
                deleted: 0,
                dependencies: Signals::default(),
                tests: 0,
                changes: BTreeMap::new(),
            });
            continue;
        }
        let changes =
            select_backend(Some(workspace.backend_kind))?.changes(workspace, false, false)?;
        let source: Vec<FileChange> = changes
            .iter()
            .filter(|change| change.class == ChangeClass::Source)
            .cloned()
            .collect();
        let signal = signals::collect(workspace, &changes);
        rows.push(CompareRow {
            workspace: workspace.name.clone(),
            pending: false,
            added: source
                .iter()
                .filter(|change| change.kind == ChangeKind::Added)
                .count(),
            modified: source
                .iter()
                .filter(|change| change.kind == ChangeKind::Modified)
                .count(),
            deleted: source
                .iter()
                .filter(|change| change.kind == ChangeKind::Deleted)
                .count(),
            tests: signal.tests_touched,
            dependencies: signal,
            changes: source
                .into_iter()
                .map(|change| (change.path, change.kind))
                .collect(),
        });
    }

    let active = rows.iter().filter(|row| !row.pending).count();
    let union: BTreeSet<_> = rows
        .iter()
        .flat_map(|row| row.changes.keys().cloned())
        .collect();
    let mut contested = Vec::new();
    let mut partial = Vec::new();
    let mut unique: Vec<_> = rows
        .iter()
        .map(|row| (row.workspace.clone(), Vec::new()))
        .collect();
    for path in union {
        let owners: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.changes.contains_key(&path))
            .map(|(index, _)| index)
            .collect();
        if active > 0 && owners.len() == active {
            contested.push(path);
        } else if owners.len() == 1 {
            unique[owners[0]].1.push(path);
        } else if owners.len() > 1 {
            partial.push(path);
        }
    }
    Ok(CompareOutput {
        base: expected.0,
        workspaces: rows,
        contested,
        unique,
        partial,
        degraded,
    })
}

fn dep_cell(signals: &Signals) -> String {
    let net = signals.deps_added as isize - signals.deps_removed as isize;
    format!("{net:+}")
}

pub fn render(output: &CompareOutput) -> String {
    let mut lines = vec![format!(
        "BASE  {}   {} workspaces, same base",
        output.base.display(),
        output.workspaces.len()
    )];
    lines.push(String::new());
    lines.push("WORKSPACE  SOURCE          DEPS  TESTS".to_string());
    for row in &output.workspaces {
        if row.pending {
            lines.push(format!("{:<10} -               -     -", row.workspace));
        } else {
            lines.push(format!(
                "{:<10} {}+ {}~ {}-   {:<5} {}",
                row.workspace,
                row.added,
                row.modified,
                row.deleted,
                dep_cell(&row.dependencies),
                row.tests
            ));
        }
    }
    if !output.contested.is_empty() {
        lines.push(format!(
            "contested (all {}): {}",
            output.workspaces.len() - output.degraded.len(),
            output
                .contested
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for (workspace, paths) in &output.unique {
        if !paths.is_empty() {
            lines.push(format!(
                "unique to {workspace}: {}",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
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

pub fn render_files(output: &CompareOutput) -> String {
    let union: BTreeSet<_> = output
        .workspaces
        .iter()
        .flat_map(|row| row.changes.keys().cloned())
        .collect();
    let mut lines = vec![format!(
        "PATH  {}",
        output
            .workspaces
            .iter()
            .map(|row| row.workspace.as_str())
            .collect::<Vec<_>>()
            .join("  ")
    )];
    for path in union {
        let cells = output
            .workspaces
            .iter()
            .map(|row| match row.changes.get(&path) {
                Some(ChangeKind::Added) => "A",
                Some(ChangeKind::Modified) => "M",
                Some(ChangeKind::Deleted) => "D",
                None => "-",
            })
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(format!("{}  {cells}", path.display()));
    }
    lines.join("\n")
}
