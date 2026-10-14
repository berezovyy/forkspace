//! The fork-time version of each file: the "fork" side of a line-level merge.
//!
//! A sealed overlay already has it in its frozen copy. Directory-snapshot workspaces keep a
//! private copy of the project's visible source and dependency files (not `node_modules`,
//! ignored, runtime, or internal paths) in `FORKSPACE_HOME/ancestors/<name>`, taken at fork
//! and refreshed by `pull`. On APFS the visible tree is cloned instead, whole directories at a
//! time; elsewhere `fs::copy` reflinks on Btrfs and XFS, so the copy is free there and small
//! elsewhere. A live overlay has no fork-time version; its overlapping edits stay whole-file
//! conflicts.

#[cfg(target_os = "macos")]
use std::collections::HashSet;
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
    crate::disk::exclude_from_backups(&root);
    let staging = root.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    if !clone_tree(from, &staging) {
        let _ = fs::remove_dir_all(&staging);
        if let Err(error) = copy_visible(from, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    }
    let destination = dir(name);
    remove(name)?;
    fs::rename(&staging, &destination)
        .io_context(|| format!("install fork-time copy {}", destination.display()))
}

/// Delete the fork-time copy of `name`, if any, in the background.
pub fn remove(name: &str) -> Result {
    crate::state::trash(&dir(name), &format!("ancestor-{name}"))
}

/// Clone the visible part of `from` to `to`. A directory with no ignored directory
/// (`node_modules`, build output) anywhere inside is cloned whole with one recursive APFS
/// `clonefile`, which shares every block and keeps mtimes; a copy per file costs ~180 us each,
/// 18 s for 100,000 files. Ignored directories are left out, so the clone stays cheap to make
/// and to delete. Ignored files inside cloned directories come along, but lookups only ever ask
/// for visible paths. False when the volume cannot clone (`to` may then need removing).
#[cfg(target_os = "macos")]
fn clone_tree(from: &Path, to: &Path) -> bool {
    let filter = ChangeFilter::build(from);
    // Directories with an ignored directory somewhere inside.
    let mut mixed = HashSet::new();
    let walker = WalkDir::new(from)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let Ok(relative) = entry.path().strip_prefix(from) else {
                return false;
            };
            if entry.depth() == 0
                || !entry.file_type().is_dir()
                || filter.is_visible(relative, true)
            {
                return true;
            }
            mixed.extend(relative.ancestors().skip(1).map(Path::to_path_buf));
            false
        });
    if walker.into_iter().any(|entry| entry.is_err()) {
        return false;
    }
    clone_visible(from, to, Path::new(""), &mixed, &filter)
}

/// Clone `from` (at `relative`) to `to`: whole unless it is `mixed`, else entry by entry,
/// skipping ignored directories.
#[cfg(target_os = "macos")]
fn clone_visible(
    from: &Path,
    to: &Path,
    relative: &Path,
    mixed: &HashSet<PathBuf>,
    filter: &ChangeFilter,
) -> bool {
    use std::os::unix::fs::PermissionsExt;

    if !mixed.contains(relative) {
        return clonefile(from, to);
    }
    let Ok(metadata) = fs::symlink_metadata(from) else {
        return false;
    };
    let (Ok(entries), Ok(())) = (fs::read_dir(from), fs::create_dir(to)) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let Ok(kind) = entry.file_type() else {
            return false;
        };
        let child = relative.join(entry.file_name());
        let wanted = if kind.is_dir() {
            filter.is_visible(&child, true)
        } else {
            kind.is_file() || kind.is_symlink()
        };
        if wanted
            && !clone_visible(
                &entry.path(),
                &to.join(entry.file_name()),
                &child,
                mixed,
                filter,
            )
        {
            return false;
        }
    }
    // Last, so a read-only directory still takes its entries.
    let mode = metadata.permissions().mode() & 0o7777;
    fs::set_permissions(to, fs::Permissions::from_mode(mode)).is_ok()
}

/// One APFS `clonefile` of `from` (recursive for a directory; a symlink itself, not its target).
#[cfg(target_os = "macos")]
fn clonefile(from: &Path, to: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let (Ok(source), Ok(target)) = (
        CString::new(from.as_os_str().as_bytes()),
        CString::new(to.as_os_str().as_bytes()),
    ) else {
        return false;
    };
    // `CLONE_NOFOLLOW` from <sys/clonefile.h>; libc does not export it.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), CLONE_NOFOLLOW) == 0 }
}

#[cfg(not(target_os = "macos"))]
fn clone_tree(_from: &Path, _to: &Path) -> bool {
    false
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
