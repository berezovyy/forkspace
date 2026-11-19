//! Every on-disk location forkspace uses, rooted at `FORKSPACE_HOME` (default `~/.forkspace`).

use std::path::PathBuf;

/// Root of all forkspace state. Overridable with `FORKSPACE_HOME`.
pub fn forkspace_home() -> PathBuf {
    if let Ok(home) = std::env::var("FORKSPACE_HOME") {
        PathBuf::from(home)
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".forkspace")
    }
}

/// Every entry forkspace keeps directly in `FORKSPACE_HOME`. An overlay workspace's root is
/// `FORKSPACE_HOME/<name>` too ([`overlay_root`]), so none of these is a workspace name.
pub const STORE_ENTRIES: &[&str] = &[
    "state.json",
    "state.lock",
    "workspaces",
    "fuse-overlayfs-probe",
    "frozen",
    "ancestors",
    "checkpoints",
    "undo",
    "trash",
    "trash-locks",
    "journal",
    "manifests",
    "logs",
    "proxy.log",
    "proxy.err",
];

/// Whether a workspace called `name` would map onto forkspace's own files (SEC-02): a store
/// entry or one of its temporary siblings (`state.json.tmp`), compared without ASCII case for
/// case-insensitive volumes, or a hidden name, the namespace of forkspace's temporary and
/// staging files (`.<name>.jsonl.tmp-<pid>`, `.<name>.tmp-<pid>`).
pub fn is_reserved_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with('.')
        || STORE_ENTRIES.iter().any(|entry| {
            name.strip_prefix(entry)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
}

/// Whether `path` is `FORKSPACE_HOME` itself or one of its reserved entries, which no
/// workspace operation may delete or move away.
pub fn is_store_path(path: &std::path::Path) -> bool {
    let home = forkspace_home();
    path == home
        || (path.parent() == Some(home.as_path())
            && path
                .file_name()
                .is_some_and(|name| is_reserved_name(&name.to_string_lossy())))
}

/// The workspace registry.
pub fn state_file() -> PathBuf {
    forkspace_home().join("state.json")
}

/// `flock` target serializing every state mutation.
pub fn state_lock() -> PathBuf {
    forkspace_home().join("state.lock")
}

/// Overlay workspaces keep `upper/`, `work/`, and `merged/` here.
pub fn overlay_root(name: &str) -> PathBuf {
    forkspace_home().join(name)
}

/// Parent directory of directory-snapshot workspaces.
pub fn snapshot_root() -> PathBuf {
    forkspace_home().join("workspaces")
}

/// Directory of the snapshot workspace `name`.
pub fn snapshot_dir(name: &str) -> PathBuf {
    snapshot_root().join(name)
}

/// What the last successful fuse-overlayfs probe ran against; see `probe_fuse_overlayfs`.
pub fn fuse_probe_cache() -> PathBuf {
    forkspace_home().join("fuse-overlayfs-probe")
}

/// Frozen copies of projects that sealed overlay workspaces mount as their lower layer.
pub fn frozen_root() -> PathBuf {
    forkspace_home().join("frozen")
}

/// Fork-time copies of snapshot workspaces' source files, for line-level merges.
pub fn ancestor_root() -> PathBuf {
    forkspace_home().join("ancestors")
}

/// Saved workspace states, one directory per workspace.
pub fn checkpoint_root() -> PathBuf {
    forkspace_home().join("checkpoints")
}

/// What each merge replaced or deleted in a project, for `merge --undo`.
pub fn undo_root() -> PathBuf {
    forkspace_home().join("undo")
}

/// Directories renamed here are deleted asynchronously.
pub fn trash_dir() -> PathBuf {
    forkspace_home().join("trash")
}

/// One lock file per trash directory, held by the process deleting it.
pub fn trash_lock_dir() -> PathBuf {
    forkspace_home().join("trash-locks")
}

/// Resumable merge and pull journals.
pub fn journal_dir() -> PathBuf {
    forkspace_home().join("journal")
}

/// Fork manifests (`<name>.jsonl`).
pub fn manifest_dir() -> PathBuf {
    forkspace_home().join("manifests")
}

/// Dev-server stdout/stderr logs for workspace `name`.
pub fn log_dir(name: &str) -> PathBuf {
    forkspace_home().join("logs").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_store_location_is_a_reserved_name() {
        let home = forkspace_home();
        for path in [
            state_file(),
            state_lock(),
            snapshot_root(),
            fuse_probe_cache(),
            frozen_root(),
            ancestor_root(),
            checkpoint_root(),
            undo_root(),
            trash_dir(),
            trash_lock_dir(),
            journal_dir(),
            manifest_dir(),
            log_dir("x").parent().unwrap().to_path_buf(),
        ] {
            let entry = path.strip_prefix(&home).unwrap().to_str().unwrap();
            assert!(is_reserved_name(entry), "{entry} is not reserved");
        }
        for name in ["workspaces-2", "my.trash", "logs2", "a", "frozenx"] {
            assert!(!is_reserved_name(name), "{name}");
        }
    }
}
