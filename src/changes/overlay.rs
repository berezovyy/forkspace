//! Change scan for overlay workspaces: the upper directory is the change set.
//!
//! Every regular entry in the upper dir is an add or a modification of the workspace's
//! ancestor (its frozen copy of the base, or the base itself for `--live`); a whiteout (a 0:0
//! character device, or a zero-length `.wh.` file) marks a deletion of the entry it names.

use std::fs;
use std::path::Path;

use walkdir::WalkDir;

use super::content::classify_file;
use super::FileChange;
use crate::backend::overlay::{is_device_whiteout, Whiteout};
use crate::error::{ForkspaceError, Result};
use crate::util::WalkDirExt;
use crate::workspace::Workspace;

pub fn collect_changes(workspace: &Workspace) -> Result<Vec<FileChange>> {
    collect(workspace, true)
}

/// Like [`collect_changes`], without line counts or binary detection.
pub fn collect_changes_fast(workspace: &Workspace) -> Result<Vec<FileChange>> {
    collect(workspace, false)
}

fn collect(workspace: &Workspace, need_line_counts: bool) -> Result<Vec<FileChange>> {
    let upper = workspace.upper_path()?;
    let ancestor = workspace.ancestor_dir();
    if !upper.exists() {
        return Ok(Vec::new());
    }

    let mut changes = Vec::new();
    for entry in WalkDir::new(upper).min_depth(1) {
        let entry = entry.walk_context(upper)?;
        let rel = entry
            .path()
            .strip_prefix(upper)
            .map_err(|_| ForkspaceError::Io {
                context: format!(
                    "entry {} is not under upper dir {}",
                    entry.path().display(),
                    upper.display()
                ),
                source: std::io::Error::other("prefix mismatch"),
            })?;
        let Some(file_name) = entry.file_name().to_str() else {
            continue;
        };

        let metadata = entry.metadata().ok();
        if metadata.as_ref().is_some_and(is_device_whiteout) {
            if fs::symlink_metadata(ancestor.join(rel)).is_ok() {
                changes.push(FileChange::deletion(rel.to_path_buf()));
            }
            continue;
        }
        if metadata.is_some_and(|metadata| metadata.len() == 0) {
            match valid_whiteout(rel, file_name, ancestor) {
                Some(Whiteout::Opaque) => continue,
                Some(Whiteout::Deletion(original_name)) => {
                    changes.push(FileChange::deletion(rel.with_file_name(original_name)));
                    continue;
                }
                None => {}
            }
        }
        if entry.file_type().is_dir() {
            continue;
        }

        let base_path = ancestor.join(rel);
        if let Some(change) = classify_file(
            rel.to_path_buf(),
            entry.path(),
            &base_path,
            need_line_counts,
        ) {
            changes.push(change);
        }
    }

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// Parse `file_name` as a whiteout. A deletion whiteout only counts when the base still
/// has the entry it hides; otherwise it is an add-then-delete with nothing to report.
fn valid_whiteout(rel: &Path, file_name: &str, base: &Path) -> Option<Whiteout> {
    match Whiteout::parse(file_name)? {
        Whiteout::Opaque => Some(Whiteout::Opaque),
        Whiteout::Deletion(original_name) => {
            let target = rel.with_file_name(&original_name);
            fs::symlink_metadata(base.join(target))
                .is_ok()
                .then_some(Whiteout::Deletion(original_name))
        }
    }
}
