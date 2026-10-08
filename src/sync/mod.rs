//! Moving changes between a workspace and its base.
//!
//! [`plan`] decides what a merge transfers and detects conflicts, [`merge`] applies it to the
//! base, and [`pull`] plans and applies the opposite direction. Both directions run through
//! [`journal`], which records the transfer as resumable per-path steps. [`install`] runs the
//! project's package manager after a transfer.

pub mod ancestor;
pub mod install;
pub mod journal;
pub mod merge;
pub mod plan;
pub mod pull;
pub mod three_way;
pub mod undo;

use std::path::PathBuf;

use crate::changes::filter::ChangeClass;
use crate::changes::{ChangeKind, FileChange};

/// Runtime and forkspace-internal paths never move between a workspace and its base.
pub(crate) fn is_transferable(class: ChangeClass) -> bool {
    !matches!(class, ChangeClass::Runtime | ChangeClass::Internal)
}

/// `a, b, c` for display in plans, warnings, and summaries.
pub(crate) fn join_paths(paths: &[PathBuf]) -> String {
    display_paths(paths).join(", ")
}

/// Each path rendered for an error payload or a joined list.
pub(crate) fn display_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect()
}

/// Render a transfer plan: a title, one `  <verb> <path>` line per change, then one
/// `<label>: <paths>` line for each non-empty note.
pub(crate) fn render_plan(
    title: String,
    changes: &[FileChange],
    verb: fn(ChangeKind) -> &'static str,
    notes: &[(&str, &[PathBuf])],
) -> String {
    let steps = changes
        .iter()
        .map(|change| format!("  {} {}", verb(change.kind), change.path.display()));
    let notes = notes
        .iter()
        .filter(|(_, paths)| !paths.is_empty())
        .map(|(label, paths)| format!("{label}: {}", join_paths(paths)));
    std::iter::once(title)
        .chain(steps)
        .chain(notes)
        .collect::<Vec<_>>()
        .join("\n")
}
