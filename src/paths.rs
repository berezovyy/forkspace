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

/// What each merge replaced or deleted in a project, for `undo`.
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

/// Where every workspace's private temporary directory lives:
/// `<system temporary directory>/forkspace-<uid>`, outside `FORKSPACE_HOME` so that it is on
/// the fast temporary filesystem and Unix socket paths inside it stay short.
pub fn private_tmp_root() -> PathBuf {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    private_tmp_root_in(&std::env::temp_dir(), uid)
}

/// [`private_tmp_root`] for the temporary directory `temp`. Inside a workspace, `temp` is
/// that workspace's own private directory, below the root: the root is then that ancestor, so
/// forkspace run from inside a workspace finds the same directories as outside it.
fn private_tmp_root_in(temp: &std::path::Path, uid: u32) -> PathBuf {
    let name = format!("forkspace-{uid}");
    temp.ancestors()
        .find(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|file| file == name.as_str())
        })
        .map_or_else(|| temp.join(&name), std::path::Path::to_path_buf)
}

/// Where the private temporary directories of this `FORKSPACE_HOME`'s workspaces live: one
/// directory per home, so two homes with a workspace of the same name never share one.
pub fn private_tmp_home() -> PathBuf {
    private_tmp_home_of(&forkspace_home())
}

/// [`private_tmp_home`] for the `FORKSPACE_HOME` `home`.
pub fn private_tmp_home_of(home: &std::path::Path) -> PathBuf {
    // FNV-1a of the home's path: short, and the same in every forkspace version.
    let tag = home
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        });
    private_tmp_root().join(format!("{:08x}", tag >> 32))
}

/// The private temporary directory of workspace `name` (`TMPDIR` inside `enter` and `run`).
pub fn private_tmp(name: &str) -> PathBuf {
    private_tmp_home().join(name)
}

/// Dev-server stdout/stderr logs for workspace `name`.
pub fn log_dir(name: &str) -> PathBuf {
    forkspace_home().join("logs").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_private_tmp_root_is_the_same_inside_a_workspace() {
        let root = std::path::Path::new("/tmp/forkspace-1000");
        assert_eq!(
            private_tmp_root_in(std::path::Path::new("/tmp"), 1000),
            root
        );
        assert_eq!(
            private_tmp_root_in(&root.join("89a6dbda/one"), 1000),
            root,
            "a workspace's TMPDIR must not nest a second root"
        );
        assert_eq!(
            private_tmp_root_in(std::path::Path::new("/tmp/forkspace-1001/x"), 1000),
            std::path::Path::new("/tmp/forkspace-1001/x/forkspace-1000"),
            "another user's directory is not ours"
        );
    }

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
