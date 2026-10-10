//! Frozen copies: immutable snapshots of a project that sealed overlay workspaces mount as
//! their lower layer.
//!
//! The live project is never an overlay layer. The kernel calls changing a lower layer under a
//! mounted overlay undefined, and in practice a branch switch, commit, or edit in the project
//! leaks into every workspace. Instead, the first fork of a project copies it once into
//! `FORKSPACE_HOME/frozen/<id>/tree` (reflinks where the filesystem shares blocks, a plain copy
//! otherwise), and later forks reuse that copy while the project is unchanged. When the project
//! has changed, the next copy hard-links every unchanged file from the previous one and copies
//! only what changed, so it costs about one directory tree. Hard links are safe because a
//! frozen copy is never written once complete: overlays copy a file up before changing it.
//!
//! Copies nothing references any more are removed by [`unreferenced`] callers in crash
//! recovery and `drop`.

use std::collections::HashSet;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::backend::snapshot::{materialize_tree, Placed};
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::process::watch::is_alive;
use crate::util::fs::{ctime_ns, ensure_private_dir, mode_bits, mtime_ns, sync_dir, write_atomic};
use crate::util::{now_unix_ns, IoContext};

const META: &str = "meta.json";
const TREE: &str = "tree";

/// A complete frozen copy.
#[derive(Debug, Clone)]
pub(crate) struct Frozen {
    /// The read-only directory to mount as a lower layer.
    pub tree: PathBuf,
    /// Whether this call created it (rather than reusing an existing copy).
    pub created: bool,
    /// Files copied (or reflinked) and hard-linked while creating it.
    pub copied_files: u64,
    pub copied_bytes: u64,
    pub linked_files: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    /// The canonical directory this copy was frozen from.
    source: PathBuf,
    /// When the scan of `source` started. An entry whose ctime is earlier and whose metadata
    /// matches the copy is identical to it.
    started_ns: u128,
    /// Entries in the tree (files, directories, and symlinks).
    entries: u64,
}

/// What a new frozen copy can share with an existing one.
pub(crate) enum Basis<'a> {
    /// `source` is a project: reuse its newest frozen copy if the project is unchanged, and
    /// otherwise hard-link the files that did not change since that copy.
    Project,
    /// `source` is the view of a sealed workspace whose lower layer is the frozen copy `lower`:
    /// every entry the workspace has not written (nothing in `upper`) is hard-linked from it.
    Workspace { lower: &'a Path, upper: &'a Path },
}

/// A frozen copy of `source`, reused or newly made.
pub(crate) fn freeze(source: &Path, basis: Basis<'_>) -> Result<Frozen> {
    let root = paths::frozen_root();
    ensure_private_dir(&root)?;
    crate::disk::exclude_from_backups(&root);
    let _lock = lock(&root, true)?;
    let source = source
        .canonicalize()
        .io_context(|| format!("canonicalize {}", source.display()))?;
    let started_ns = now_unix_ns();

    match basis {
        Basis::Project => {
            let previous = newest_for(&root, &source);
            if let Some((tree, meta)) = &previous {
                if unchanged_since(&source, tree, meta)? {
                    return Ok(Frozen {
                        tree: tree.clone(),
                        created: false,
                        copied_files: 0,
                        copied_bytes: 0,
                        linked_files: 0,
                    });
                }
            }
            build(&root, &source, started_ns, |relative, path| {
                let Some((tree, meta)) = &previous else {
                    return None;
                };
                let candidate = tree.join(relative);
                let source_meta = fs::symlink_metadata(path).ok()?;
                let copy_meta = fs::symlink_metadata(&candidate).ok()?;
                same_file(&source_meta, &copy_meta, meta.started_ns).then_some(candidate)
            })
        }
        Basis::Workspace { lower, upper } => build(&root, &source, started_ns, |relative, _| {
            let written = fs::symlink_metadata(upper.join(relative)).is_ok();
            (!written).then(|| lower.join(relative))
        }),
    }
}

/// The ids (directory names under the frozen root) of the frozen copies `trees` live in.
pub(crate) fn ids_of<'a>(trees: impl IntoIterator<Item = &'a Path>) -> HashSet<String> {
    trees
        .into_iter()
        .filter_map(|tree| tree.parent()?.file_name()?.to_str().map(str::to_string))
        .collect()
}

/// Frozen copies and abandoned partial copies that `referenced` (ids from [`ids_of`]) does not
/// keep alive. Returns nothing when another process is creating a copy right now.
pub(crate) fn unreferenced(referenced: &HashSet<String>) -> Vec<PathBuf> {
    let root = paths::frozen_root();
    let Ok(_lock) = lock(&root, false) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut garbage = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !entry.path().is_dir() {
            continue;
        }
        if let Some(staging) = name.strip_prefix('.') {
            if !staging_pid(staging).is_some_and(|pid| is_alive(pid, None)) {
                garbage.push(entry.path());
            }
        } else if !referenced.contains(&name) {
            garbage.push(entry.path());
        }
    }
    garbage
}

/// The pid in a staging name `<started>-<pid>.tmp`.
fn staging_pid(name: &str) -> Option<u32> {
    name.strip_suffix(".tmp")?.rsplit_once('-')?.1.parse().ok()
}

/// Build a new frozen copy of `source`. `link_from(relative, path)` names an identical file
/// of an existing frozen copy to hard-link instead of copying.
fn build(
    root: &Path,
    source: &Path,
    started_ns: u128,
    link_from: impl Fn(&Path, &Path) -> Option<PathBuf>,
) -> Result<Frozen> {
    let id = format!("{started_ns:x}-{}", std::process::id());
    let staging = root.join(format!(".{id}.tmp"));
    let tree = staging.join(TREE);
    let reflink = reflink_available(source, root);

    let materialized = materialize_tree(source, &tree, |path, target| {
        let relative = path.strip_prefix(source).map_err(|_| {
            ForkspaceError::StateFailed("frozen copy walk escaped its source".to_string())
        })?;
        if let Some(existing) = link_from(relative, path) {
            if fs::hard_link(&existing, target).is_ok() {
                return Ok(Placed::Linked);
            }
        }
        place(path, target, reflink)
    });
    let stats = match materialized {
        Ok(stats) => stats,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    let meta = Meta {
        source: source.to_path_buf(),
        started_ns,
        entries: stats.entries,
    };
    write_atomic(&staging.join(META), &serde_json::to_vec_pretty(&meta)?)?;
    let finished = root.join(&id);
    fs::rename(&staging, &finished)
        .io_context(|| format!("install frozen copy {}", finished.display()))?;
    sync_dir(root)?;
    Ok(Frozen {
        tree: finished.join(TREE),
        created: true,
        copied_files: stats.copied_files + stats.reflinked_files,
        copied_bytes: stats.copied_bytes,
        linked_files: stats.linked_files,
    })
}

#[cfg(target_os = "linux")]
fn reflink_available(source: &Path, root: &Path) -> bool {
    crate::backend::snapshot::reflink_probe(source, root).available
}

#[cfg(not(target_os = "linux"))]
fn reflink_available(_source: &Path, _root: &Path) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn place(path: &Path, target: &Path, reflink: bool) -> Result<Placed> {
    if reflink {
        crate::backend::snapshot::reflink_file(path, target)
    } else {
        Ok(Placed::No)
    }
}

#[cfg(not(target_os = "linux"))]
fn place(_path: &Path, _target: &Path, _reflink: bool) -> Result<Placed> {
    Ok(Placed::No)
}

/// The newest complete frozen copy of `source`, with its metadata.
fn newest_for(root: &Path, source: &Path) -> Option<(PathBuf, Meta)> {
    fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| {
            let meta: Meta =
                serde_json::from_slice(&fs::read(entry.path().join(META)).ok()?).ok()?;
            let tree = entry.path().join(TREE);
            (meta.source == source && tree.is_dir()).then_some((tree, meta))
        })
        .max_by_key(|(_, meta)| meta.started_ns)
}

/// Whether `source` still matches the frozen copy `tree` entry for entry.
fn unchanged_since(source: &Path, tree: &Path, meta: &Meta) -> Result<bool> {
    let differs = AtomicBool::new(false);
    let seen = crate::util::walk::parallel(source, |relative, path, current| {
        if differs.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let same = fs::symlink_metadata(tree.join(relative)).is_ok_and(|frozen| {
            if current.is_dir() {
                frozen.is_dir() && mode_bits(current) == mode_bits(&frozen)
            } else if current.file_type().is_symlink() {
                frozen.file_type().is_symlink()
                    && fs::read_link(path).ok() == fs::read_link(tree.join(relative)).ok()
            } else {
                same_file(current, &frozen, meta.started_ns)
            }
        });
        if !same {
            differs.store(true, Ordering::Relaxed);
        }
        Ok(Some(()))
    })?;
    Ok(!differs.load(Ordering::Relaxed) && seen.len() as u64 == meta.entries)
}

/// Whether a project file (`current`) is still identical to its frozen copy (`frozen`): a
/// regular file with the same size, mtime, and mode, not changed since the copy was taken.
fn same_file(current: &fs::Metadata, frozen: &fs::Metadata, started_ns: u128) -> bool {
    current.is_file()
        && frozen.is_file()
        && ctime_ns(current) < i128::try_from(started_ns).unwrap_or(i128::MAX)
        && current.len() == frozen.len()
        && mtime_ns(current) == mtime_ns(frozen)
        && mode_bits(current) == mode_bits(frozen)
}

/// An exclusive lock on the frozen store; `blocking: false` fails instead of waiting.
fn lock(root: &Path, blocking: bool) -> Result<fs::File> {
    fs::create_dir_all(root).io_context(|| format!("create {}", root.display()))?;
    let path = root.join(".lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .io_context(|| format!("open {}", path.display()))?;
    let operation = if blocking {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
        return Err(ForkspaceError::LockFailed(format!(
            "lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(file)
}
