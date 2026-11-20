//! What forkspace keeps beside workspaces and no longer needs, removed without asking: frozen
//! copies no workspace uses, undo data and snapshots past their retention, fork-time copies and
//! snapshots of workspaces that are gone, a merge's staging that lost its journal, systemd
//! slices and private temporary directories of workspaces that no longer exist. Workspaces
//! themselves are never removed here; `ls` shows how old each one is.
//!
//! Called with the state lock held (`drop` runs it), so nothing it judges unused can come back
//! into use while it removes it.

use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;
use crate::process::systemd;
use crate::state::State;
use crate::sync::{checkpoint, undo};

/// Remove every leftover; failures are left for the next time.
pub fn leftovers(state: &State) {
    let mut directories: Vec<PathBuf> = state.unused_frozen_copies();
    directories.extend(undo::expired());
    for name in state.workspaces.keys() {
        directories.extend(checkpoint::expired(name));
    }
    for root in [paths::ancestor_root(), paths::checkpoint_root()] {
        for path in subdirectories(&root) {
            if !state.name_in_use(&owner(&path)) {
                directories.push(path);
            }
        }
    }
    for path in subdirectories(&paths::journal_dir()) {
        if path.extension().is_some_and(|extension| extension == "d")
            && !path.with_extension("json").exists()
        {
            directories.push(path);
        }
    }
    for path in directories {
        let _ = fs::remove_dir_all(path);
    }
    for unit in systemd::orphan_slices(state.workspaces.keys()) {
        systemd::stop_unit(&unit);
    }
    let names: Vec<&str> = state.workspaces.keys().map(String::as_str).collect();
    super::tmp::sweep(&names);
}

/// The workspace a store entry belongs to: its name, or the name in a temporary
/// `.<name>.tmp-<pid>` spelling.
fn owner(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    file_name
        .trim_start_matches('.')
        .split(".tmp-")
        .next()
        .unwrap_or(file_name)
        .to_string()
}

fn subdirectories(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default()
}
