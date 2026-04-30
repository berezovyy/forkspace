//! Moving changes between a workspace and its base.
//!
//! [`plan`] decides what a merge transfers and detects conflicts, [`merge`] applies it to the
//! base, and [`pull`] plans and applies the opposite direction. Both directions run through
//! [`journal`], which records the transfer as resumable per-path steps. [`install`] runs the
//! project's package manager after a transfer.

pub mod install;
pub mod journal;
pub mod merge;
pub mod plan;
pub mod pull;

use std::path::PathBuf;

use crate::changes::filter::ChangeClass;
use crate::changes::manifest::ManifestKind;
use crate::changes::{ChangeKind, FileChange};

/// How many changes of each kind a set of changes contains.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
}

impl ChangeCounts {
    pub fn of<'a>(changes: impl IntoIterator<Item = &'a FileChange>) -> Self {
        changes
            .into_iter()
            .fold(Self::default(), |mut counts, change| {
                match change.kind {
                    ChangeKind::Added => counts.added += 1,
                    ChangeKind::Modified => counts.modified += 1,
                    ChangeKind::Deleted => counts.deleted += 1,
                }
                counts
            })
    }

    pub fn total(&self) -> usize {
        self.added + self.modified + self.deleted
    }
}

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

/// The manifest kind of an entry, from its `symlink_metadata`.
pub(crate) fn manifest_kind(metadata: &std::fs::Metadata) -> ManifestKind {
    if metadata.file_type().is_symlink() {
        ManifestKind::Symlink
    } else if metadata.is_dir() {
        ManifestKind::Dir
    } else {
        ManifestKind::File
    }
}
