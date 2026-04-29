//! Change scan for overlay workspaces: the upper directory is the change set.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use super::content::classify_file;
use super::FileChange;
use crate::backend::overlay::mount::Whiteout;
use crate::error::{ForkspaceError, Result};
use crate::util::WalkDirExt;
use crate::workspace::Workspace;

fn whiteout_target_path(rel: &Path, original_name: &str) -> PathBuf {
    if let Some(parent) = rel.parent() {
        parent.join(original_name)
    } else {
        PathBuf::from(original_name)
    }
}

fn valid_whiteout(rel: &Path, file_name: &str, base: &Path) -> Option<Whiteout> {
    match Whiteout::parse(file_name)? {
        Whiteout::Opaque => Some(Whiteout::Opaque),
        Whiteout::Deletion(original_name) => {
            let target = whiteout_target_path(rel, &original_name);
            fs::symlink_metadata(base.join(target))
                .is_ok()
                .then_some(Whiteout::Deletion(original_name))
        }
    }
}

pub fn collect_changes(workspace: &Workspace) -> Result<Vec<FileChange>> {
    collect_changes_inner(workspace, true)
}

pub fn collect_changes_fast(workspace: &Workspace) -> Result<Vec<FileChange>> {
    collect_changes_inner(workspace, false)
}

fn collect_changes_inner(workspace: &Workspace, need_line_counts: bool) -> Result<Vec<FileChange>> {
    let upper = workspace.upper_path()?;
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

        let file_name = match entry.file_name().to_str() {
            Some(n) => n,
            None => continue,
        };

        if entry.metadata().is_ok_and(|metadata| metadata.len() == 0) {
            if let Some(whiteout) = valid_whiteout(rel, file_name, &workspace.base) {
                match whiteout {
                    Whiteout::Opaque => continue,
                    Whiteout::Deletion(original_name) => {
                        changes.push(FileChange::deletion(whiteout_target_path(
                            rel,
                            &original_name,
                        )));
                        continue;
                    }
                }
            }
        }

        if entry.file_type().is_dir() {
            continue;
        }

        let rel_path = rel.to_path_buf();
        let base_path = workspace.base.join(&rel_path);
        let upper_path = entry.path().to_path_buf();

        if let Some(change) = classify_file(rel_path, &upper_path, &base_path, need_line_counts) {
            changes.push(change);
        }
    }

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}
