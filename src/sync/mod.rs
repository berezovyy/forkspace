//! Moving changes between a workspace and its base.
//!
//! [`plan`] decides what a merge transfers and detects conflicts, [`merge`] applies it to the
//! base, and [`pull`] plans and applies the opposite direction. Both directions run through
//! [`journal`], which records the transfer as resumable per-path steps. [`install`] runs the
//! project's package manager after a transfer.

pub mod ancestor;
pub mod checkpoint;
pub mod install;
pub mod journal;
pub mod merge;
pub mod plan;
pub mod pull;
pub mod three_way;
pub mod undo;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::changes::filter::ChangeClass;
use crate::changes::{ChangeKind, FileChange};
use crate::util::fs::exists_exact;

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

/// Move each deletion that removes, under another spelling, an entry of `target` that
/// additions recreate to just before those additions; return the added paths (or their
/// directories) that are such respellings.
///
/// A case- or normalization-insensitive volume (APFS) resolves `Foo.txt` and `foo.txt`, or a
/// precomposed and a decomposed `café`, to one entry. A rename between two spellings is a
/// deletion plus an addition of that entry: applied in the other order, the deletion would
/// remove what the addition just wrote, and the addition would look like a clash with an
/// entry the target already has.
pub(crate) fn order_respellings(target: &Path, changes: &mut Vec<FileChange>) -> Vec<PathBuf> {
    let identity = |path: &Path| {
        fs::symlink_metadata(target.join(path))
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()))
    };
    let deleted: HashMap<_, PathBuf> = changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Deleted)
        .filter_map(|change| Some((identity(&change.path)?, change.path.clone())))
        .collect();
    if deleted.is_empty() {
        return Vec::new();
    }
    let mut pairs = Vec::new();
    let mut checked = HashSet::new();
    for change in changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Added)
    {
        for spelling in change.path.ancestors() {
            if spelling.as_os_str().is_empty() || !checked.insert(spelling.to_path_buf()) {
                break;
            }
            let Some(old) = identity(spelling).and_then(|id| deleted.get(&id)) else {
                continue;
            };
            if old != spelling && !exists_exact(target, spelling) {
                pairs.push((old.clone(), spelling.to_path_buf()));
            }
        }
    }
    for (old, new) in &pairs {
        let Some(from) = changes
            .iter()
            .position(|change| change.kind == ChangeKind::Deleted && &change.path == old)
        else {
            continue;
        };
        let deletion = changes.remove(from);
        let to = changes
            .iter()
            .position(|change| change.path.starts_with(new))
            .unwrap_or(changes.len());
        changes.insert(to, deletion);
    }
    pairs.into_iter().map(|(_, new)| new).collect()
}
