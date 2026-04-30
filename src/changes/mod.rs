//! Change detection: what differs between a workspace view and its base.
//!
//! Overlay workspaces are scanned from their upper directory ([`overlay`]); directory
//! snapshots are compared against the fork manifest ([`snapshot`]). Both produce the same
//! [`FileChange`] list, which [`render`] turns into CLI output.

mod content;
mod cumulative;
pub mod filter;
pub mod manifest;
mod overlay;
mod path_serde;
pub mod render;
mod snapshot;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use filter::{ChangeClass, ChangeFilter};

pub use content::is_binary;
pub use cumulative::{collect_cumulative_changes, collect_cumulative_changes_fast};
pub use overlay::{collect_changes, collect_changes_fast};
pub use render::{format_diff, generate_unified_diff, DiffFormat, DiffOutput};
pub use snapshot::collect_clone_changes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One changed path in a workspace, relative to the workspace root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    #[serde(with = "path_serde")]
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub class: ChangeClass,
    /// Lines added; `None` when line counts were not requested or do not apply.
    pub additions: Option<usize>,
    /// Lines removed; `None` when line counts were not requested or do not apply.
    pub deletions: Option<usize>,
    pub is_binary: bool,
    #[serde(skip, default)]
    pub old_content: OldContent,
}

/// Whether the pre-change content of a path can still be read from the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OldContent {
    /// The base still holds the content the workspace forked from.
    #[default]
    Available,
    /// The base was modified after the fork, so it no longer shows the ancestor.
    BaseAdvanced,
    /// The fork-time content was never recorded (manifest pending or entry dirty at fork).
    AncestorUnknown,
}

impl FileChange {
    /// A change with no line counts, not binary, and its old content available.
    pub fn new(path: PathBuf, kind: ChangeKind) -> Self {
        Self {
            class: ChangeFilter::class(&path),
            path,
            kind,
            additions: None,
            deletions: None,
            is_binary: false,
            old_content: OldContent::Available,
        }
    }

    pub(crate) fn deletion(path: PathBuf) -> Self {
        Self::new(path, ChangeKind::Deleted)
    }
}
