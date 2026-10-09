//! The fork-time version of each file: the "fork" side of a line-level merge.
//!
//! A sealed overlay already has it in its frozen copy. Directory-snapshot workspaces keep a
//! private copy of the project's visible source and dependency files (not `node_modules`,
//! ignored, runtime, or internal paths) in `FORKSPACE_HOME/ancestors/<name>`, taken at fork
//! and refreshed by `pull`. `fs::copy` clones on APFS, Btrfs, and XFS, so the copy is free
//! there and small elsewhere. A live overlay has no fork-time version; its overlapping edits
//! stay whole-file conflicts.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::backend::BackendState;
use crate::changes::filter::ChangeFilter;
use crate::error::Result;
use crate::paths;
use crate::util::fs::ensure_private_dir;
use crate::util::{IoContext, WalkDirExt};
use crate::workspace::Workspace;

/// Files larger than this are not kept: they are never merged line by line.
const MAX_FILE_BYTES: u64 = 8 << 20;

/// The fork-time version of `relative` in `workspace`, if one is known and is a regular file.
pub fn file(workspace: &Workspace, relative: &Path) -> Option<PathBuf> {
    let root = match &workspace.backend_state {
        BackendState::Overlay(layers) if layers.lower.is_some() => {
            workspace.ancestor_dir().to_path_buf()
        }
        BackendState::Overlay(_) => return None,
        BackendState::MacClone { .. } | BackendState::CowDir { .. } => dir(&workspace.name),
    };
    let path = root.join(relative);
    fs::symlink_metadata(&path)
        .is_ok_and(|metadata| metadata.is_file())
        .then_some(path)
}

/// Where the fork-time copy of snapshot workspace `name` lives.
pub fn dir(name: &str) -> PathBuf {
    paths::ancestor_root().join(name)
}

/// Record `from` (the fresh view, or the base after a pull) as the fork-time version of
/// snapshot workspace `name`, replacing any earlier copy.
pub fn record(name: &str, from: &Path) -> Result {
    let root = paths::ancestor_root();
    ensure_private_dir(&root)?;
    let staging = root.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    if let Err(error) = copy_visible(from, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    let destination = dir(name);
    remove(name)?;
    fs::rename(&staging, &destination)
        .io_context(|| format!("install fork-time copy {}", destination.display()))
}

/// Delete the fork-time copy of `name`, if any.
pub fn remove(name: &str) -> Result {
    let path = dir(name);
    match fs::remove_dir_all(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).io_context(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Copy the regular files of `from` that a diff would show into `to`.
fn copy_visible(from: &Path, to: &Path) -> Result {
    let filter = ChangeFilter::build(from);
    let walker = WalkDir::new(from)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_dir()
                || entry
                    .path()
                    .strip_prefix(from)
                    .is_ok_and(|relative| filter.is_visible(relative, true))
        });
    fs::create_dir_all(to).io_context(|| format!("create {}", to.display()))?;
    for entry in walker.skip(1) {
        let entry = entry.walk_context(from)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(from) else {
            continue;
        };
        let small = entry
            .metadata()
            .is_ok_and(|metadata| metadata.len() <= MAX_FILE_BYTES);
        if !small || !filter.is_visible(relative, false) {
            continue;
        }
        let target = to.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).io_context(|| format!("create {}", parent.display()))?;
        }
        fs::copy(entry.path(), &target)
            .io_context(|| format!("copy {} to {}", entry.path().display(), target.display()))?;
        // Keep the modification time, so restoring a fork-time version is exact.
        if let Ok(modified) = entry
            .metadata()
            .map_err(std::io::Error::from)
            .and_then(|m| m.modified())
        {
            fs::OpenOptions::new()
                .write(true)
                .open(&target)
                .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
                .io_context(|| format!("set time on {}", target.display()))?;
        }
    }
    Ok(())
}
