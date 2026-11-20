//! A private temporary directory per workspace.
//!
//! Commands started by `enter` and `run` get `TMPDIR` (and `TMP`, `TEMP`) pointing at a
//! directory of their workspace's own. Parallel agents running the same tests then no longer
//! meet in fixed `/tmp` paths (sockets, lock files, build scratch), and what a killed or
//! timed-out command leaves there goes with the workspace on `drop`.
//! `FORKSPACE_PRIVATE_TMP=0` turns it off.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::paths;

fn disabled() -> bool {
    std::env::var("FORKSPACE_PRIVATE_TMP")
        .is_ok_and(|value| matches!(value.as_str(), "0" | "off" | "false" | "no"))
}

/// The private temporary directory of `name`, created if needed; `None` when turned off or
/// when it cannot be made safely (a warning says why).
pub fn ensure(name: &str) -> Option<PathBuf> {
    if disabled() {
        return None;
    }
    let root = paths::private_tmp_root();
    let directory = paths::private_tmp(name);
    match owned_private_dir(&root).and_then(|()| owned_private_dir(&directory)) {
        Ok(()) => Some(directory),
        Err(reason) => {
            eprintln!(
                "warning: no private TMPDIR for '{name}' ({reason}); commands use the shared one"
            );
            None
        }
    }
}

/// Make `path` a directory only its owner can use, or check that it already is one: in a
/// shared `/tmp`, another user could have created it first, or put a symlink there.
fn owned_private_dir(path: &Path) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create {}: {error}", path.display())),
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(format!(
            "{} is not a directory of this user",
            path.display()
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}

/// The environment that points temporary files at `directory`.
pub fn env(directory: &Path) -> Vec<(String, String)> {
    let value = directory.display().to_string();
    ["TMPDIR", "TMP", "TEMP"]
        .into_iter()
        .map(|key| (key.to_string(), value.clone()))
        .collect()
}

/// Remove the private temporary directory of `name`, if any.
pub fn remove(name: &str) {
    let directory = paths::private_tmp(name);
    if fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
        let _ = fs::remove_dir_all(&directory);
    }
}

/// Remove the private temporary directories of workspaces that no longer exist, keeping
/// those named in `keep`.
pub fn sweep(keep: &[&str]) {
    let Ok(entries) = fs::read_dir(paths::private_tmp_root()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if keep.iter().any(|kept| name == **kept) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}
