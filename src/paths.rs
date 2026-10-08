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

/// Frozen copies of projects that sealed overlay workspaces mount as their lower layer.
pub fn frozen_root() -> PathBuf {
    forkspace_home().join("frozen")
}

/// Fork-time copies of snapshot workspaces' source files, for line-level merges.
pub fn ancestor_root() -> PathBuf {
    forkspace_home().join("ancestors")
}

/// Directories renamed here are deleted asynchronously.
pub fn trash_dir() -> PathBuf {
    forkspace_home().join("trash")
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
