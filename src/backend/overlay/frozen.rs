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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::backend::snapshot::{materialize_listed, materialize_recorded, Placed};
use crate::changes::manifest::{self, Manifest};
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::process::watch::is_alive;
use crate::util::fs::{ensure_private_dir, sync_dir, write_atomic};
use crate::util::walk::Entry;
use crate::util::{now_unix_ns, IoContext};

const META: &str = "meta.json";
const TREE: &str = "tree";
const MANIFEST: &str = "manifest.jsonl";
/// Created once a background flush has put the copy on disk; see [`flush_in_background`].
const DURABLE: &str = "durable";
/// Created when a copy left unflushed by a crash did not match its manifest.
const DAMAGED: &str = "damaged";

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
    /// The boot the copy was written in, for copies flushed in the background (older copies
    /// were flushed before they were used, and have none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boot: Option<String>,
    /// For a delta (see [`freeze_delta`]): the tree of the full frozen copy it builds on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<PathBuf>,
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
            // The previous copy's manifest answers "is this file unchanged?" without an
            // `lstat` of it; the walk already read the project file's facts.
            let previous_rows = previous
                .as_ref()
                .and_then(|(tree, _)| Manifest::open_file(&manifest_cache(tree)).ok());
            build(&root, &source, started_ns, None, |relative, path, entry| {
                let (tree, meta) = previous.as_ref()?;
                let candidate = tree.join(relative);
                let unchanged = match &previous_rows {
                    Some(rows) => rows
                        .get(&manifest::encode_path(relative))
                        .is_some_and(|row| {
                            row.kind == manifest::ManifestKind::File
                                && same_as_row(path, entry, &row, meta.started_ns)
                        }),
                    None => Entry::lstat(&candidate)
                        .is_ok_and(|copy| same_file(entry, &copy, meta.started_ns)),
                };
                unchanged.then_some(candidate)
            })
        }
        Basis::Workspace { lower, upper } => {
            // The view is a FUSE mount: reading all of it costs a round trip per entry. Its
            // layers are plain directories, and the lower one carries its manifest.
            let listed = match Manifest::open_file(&manifest_cache(lower)) {
                Ok(rows) => layered_entries(&source, lower, upper, &rows)?,
                Err(_) => None,
            };
            build(&root, &source, started_ns, listed, |relative, _, _| {
                let written = fs::symlink_metadata(upper.join(relative)).is_ok();
                (!written).then(|| lower.join(relative))
            })
        }
    }
}

/// Where the fork manifest of the frozen copy `tree` is kept once a fork has recorded it. A
/// frozen copy never changes, so every fork that mounts it would record the same rows; later
/// forks copy this file instead of walking the copy again.
pub(crate) fn manifest_cache(tree: &Path) -> PathBuf {
    tree.parent().unwrap_or(tree).join(MANIFEST)
}

/// Keep `manifest`, recorded by a fork of the frozen copy `tree`, for later forks of it. The
/// file is complete and durable before it gets its name, so a reader never sees a partial one.
pub(crate) fn save_manifest_cache(tree: &Path, manifest: &Path) -> Result {
    let cache = manifest_cache(tree);
    let temporary = cache.with_file_name(format!(".{MANIFEST}.tmp-{}", std::process::id()));
    let saved = fs::copy(manifest, &temporary)
        .io_context(|| format!("copy manifest {}", manifest.display()))
        .and_then(|_| crate::util::fs::sync_file(&temporary))
        .and_then(|()| {
            fs::rename(&temporary, &cache)
                .io_context(|| format!("install manifest cache {}", cache.display()))
        });
    if saved.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    saved
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

/// Build a new frozen copy of `source`. `link_from(relative, path, facts)` names an identical
/// file of an existing frozen copy to hard-link instead of copying. `listed` describes the
/// tree when that is cheaper than walking `source` (see [`layered_entries`]).
fn build(
    root: &Path,
    source: &Path,
    started_ns: u128,
    listed: Option<Vec<(PathBuf, Entry, PathBuf)>>,
    link_from: impl Fn(&Path, &Path, &Entry) -> Option<PathBuf> + Sync,
) -> Result<Frozen> {
    let id = format!("{started_ns:x}-{}", std::process::id());
    let staging = root.join(format!(".{id}.tmp"));
    let tree = staging.join(TREE);
    let reflink = reflink_available(source, root);
    // With a boot id the flush can happen in the background: a copy a crash leaves unflushed
    // is checked before anything trusts it again (see [`settle`]).
    let boot = boot_id();

    // Where a file lands in the copy names it; where it is read from may be another layer.
    let place_file = |path: &Path, target: &Path, entry: &Entry| {
        let relative = target.strip_prefix(&tree).map_err(|_| {
            ForkspaceError::StateFailed("frozen copy entry escaped its tree".to_string())
        })?;
        if let Some(existing) = link_from(relative, path, entry) {
            if fs::hard_link(&existing, target).is_ok() {
                return Ok(Placed::Linked);
            }
        }
        place(path, target, reflink)
    };
    let materialized = match listed {
        Some(listed) => materialize_listed(listed, &tree, place_file, boot.is_none()),
        None => materialize_recorded(source, &tree, place_file, boot.is_none()),
    };
    let (stats, entries) = match materialized {
        Ok(materialized) => materialized,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    // Every fork of this copy records the same manifest; write it now, from what the copy
    // already knows, instead of walking the copy again (see `manifest_cache`).
    // A copy flushed in the background takes its manifest along (`settle` checks both).
    if let Err(error) = write_manifest_cache(&staging, &tree, &entries, boot.is_none()) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    let meta = Meta {
        source: source.to_path_buf(),
        started_ns,
        entries: stats.entries,
        boot: boot.clone(),
        base: None,
    };
    write_atomic(&staging.join(META), &serde_json::to_vec_pretty(&meta)?)?;
    let finished = root.join(&id);
    fs::rename(&staging, &finished)
        .io_context(|| format!("install frozen copy {}", finished.display()))?;
    sync_dir(root)?;
    let own_files: u64 = [
        finished.clone(),
        finished.join(TREE),
        finished.join(META),
        finished.join(MANIFEST),
    ]
    .iter()
    .filter_map(|path| Entry::lstat(path).ok())
    .map(|entry| entry.blocks * 512)
    .sum();
    crate::disk::note_frozen_copy(&id, stats.allocated_bytes + own_files);
    if boot.is_some() {
        PENDING_FLUSHES
            .lock()
            .expect("pending flushes")
            .push(finished.clone());
    }
    Ok(Frozen {
        tree: finished.join(TREE),
        created: true,
        copied_files: stats.copied_files + stats.reflinked_files,
        copied_bytes: stats.copied_bytes,
        linked_files: stats.linked_files,
    })
}

/// The entries of the overlay view `view` whose upper layer is `upper` and whose lower layer
/// is the frozen copy `lower` with the manifest `rows`, in path order, each with where to
/// read it. Only directories the upper layer has are read through the view, so fuse-overlayfs
/// itself decides what they show (whiteouts, opaque directories); every directory the upper
/// layer lacks is exactly the lower one, described by its manifest without reading it.
/// `None` if the layers disagree with the view, so the caller walks the view instead.
fn layered_entries(
    view: &Path,
    lower: &Path,
    upper: &Path,
    rows: &Manifest,
) -> Result<Option<Vec<(PathBuf, Entry, PathBuf)>>> {
    use super::whiteout::is_device_whiteout;

    let mut listed = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(directory) = pending.pop() {
        let shown = view.join(&directory);
        let children = fs::read_dir(&shown).io_context(|| format!("read {}", shown.display()))?;
        for child in children {
            let child = child.io_context(|| format!("read {}", shown.display()))?;
            let relative = directory.join(child.file_name());
            let written = upper.join(&relative);
            let in_upper =
                fs::symlink_metadata(&written).is_ok_and(|metadata| !is_device_whiteout(&metadata));
            if in_upper {
                let facts = Entry::lstat(&written)?;
                if facts.is_dir() {
                    pending.push(relative.clone());
                }
                listed.push((relative, facts, written));
                continue;
            }
            let encoded = manifest::encode_path(&relative);
            let Some(row) = rows.get(&encoded) else {
                return Ok(None);
            };
            listed.push((relative.clone(), row_entry(&row), lower.join(&relative)));
            if row.kind == manifest::ManifestKind::Dir {
                for below in rows.under(&encoded) {
                    let relative = manifest::decode_path(below.path)?;
                    let source = lower.join(&relative);
                    listed.push((relative, row_entry(&below), source));
                }
            }
        }
    }
    listed.sort_by(|a, b| {
        a.0.as_os_str()
            .as_encoded_bytes()
            .cmp(b.0.as_os_str().as_encoded_bytes())
    });
    Ok(Some(listed))
}

/// The facts a manifest row records, as a walk would have read them.
fn row_entry(row: &manifest::RowRef<'_>) -> Entry {
    use crate::util::walk::Kind;

    Entry {
        kind: match row.kind {
            manifest::ManifestKind::File => Kind::File,
            manifest::ManifestKind::Dir => Kind::Dir,
            manifest::ManifestKind::Symlink => Kind::Symlink,
        },
        len: row.size,
        mode: row.mode,
        mtime_ns: row.mtime_ns,
        ctime_ns: 0,
        dev: 0,
        ino: 0,
        nlink: 1,
        blocks: 0,
    }
}

/// Write the manifest of the copy `tree` (being built in `staging`) from the entries
/// [`materialize_recorded`] returned. Nothing is written when a path has a `.wh.` component:
/// such a copy cannot be mounted, and the fork that records its manifest refuses it.
fn write_manifest_cache(
    staging: &Path,
    tree: &Path,
    entries: &[(PathBuf, Entry)],
    durable: bool,
) -> Result {
    let mut rows = Vec::with_capacity(entries.len());
    for (relative, entry) in entries {
        // No fork has happened yet, so nothing counts as written after one.
        let row = manifest::row_of(relative, &tree.join(relative), entry, u128::MAX)?;
        if has_whiteout_name(&row.path) {
            return Ok(());
        }
        rows.push(row);
    }
    let temporary = staging.join(format!(".{MANIFEST}.tmp-{}", std::process::id()));
    manifest::write_file(&staging.join(MANIFEST), &temporary, rows, durable)?;
    Ok(())
}

/// Whether an encoded manifest path has a component fuse-overlayfs reserves for its deletion
/// markers.
pub(crate) fn has_whiteout_name(encoded: &str) -> bool {
    encoded
        .split('/')
        .any(|component| component.starts_with(".wh."))
}

/// This boot's id, or `None` where the system has none to read.
fn boot_id() -> Option<String> {
    let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Frozen copies this process built and has not started flushing yet.
static PENDING_FLUSHES: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Start flushing the frozen copies this command built; `main` calls it once the command has
/// finished. Waiting until then keeps the command's own small `fsync`s (manifest, state) from
/// queueing behind hundreds of megabytes of copy data.
pub fn flush_pending() {
    let pending = std::mem::take(&mut *PENDING_FLUSHES.lock().expect("pending flushes"));
    for dir in pending {
        flush_in_background(&dir);
    }
}

/// Flush the finished copy `dir` to disk in a detached process and mark it durable once that
/// is done, so the fork does not wait for hundreds of megabytes to reach the disk. If the
/// process cannot be started, flush here instead.
fn flush_in_background(dir: &Path) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new("sh");
    command
        .args(["-c", "sync -f \"$1\" && : > \"$1/durable\"", "sh"])
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the hook runs between fork and exec and only calls `setsid`, which is
    // async-signal-safe; the new session keeps the flush alive after forkspace exits.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    if command.spawn().is_err() && sync_filesystem(dir).is_ok() {
        let _ = fs::write(dir.join(DURABLE), b"");
    }
}

fn sync_filesystem(dir: &Path) -> Result {
    #[cfg(target_os = "linux")]
    {
        let directory = fs::File::open(dir).io_context(|| format!("open {}", dir.display()))?;
        if unsafe { libc::syncfs(directory.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .io_context(|| format!("sync the filesystem holding {}", dir.display()));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        unsafe { libc::sync() };
    }
    Ok(())
}

/// Whether the copy in `dir` (with `meta`) can be used: flushed before it was used (older
/// copies), flushed in the background, or written in this boot, so whatever is not on disk
/// yet is still in memory. A copy a crash left unflushed is not, until [`settle`] checks it.
fn trusted(dir: &Path, meta: &Meta) -> bool {
    if dir.join(DAMAGED).exists() {
        return false;
    }
    match &meta.boot {
        None => true,
        Some(boot) => dir.join(DURABLE).exists() || boot_id().as_ref() == Some(boot),
    }
}

/// What [`settle`] found about a frozen copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settled {
    /// Durable, or written in this boot: nothing to check.
    Trusted,
    /// Left unflushed by a crash but intact; now flushed and marked durable.
    Verified,
    /// Left unflushed by a crash and not as its manifest describes it.
    Damaged,
}

/// Check the frozen copy whose tree is `tree` after a crash left it unflushed: every entry
/// must still be as its manifest recorded it. An intact copy is flushed and marked durable;
/// a damaged one is marked, so nothing reuses it.
pub(crate) fn settle(tree: &Path) -> Settled {
    let Some(dir) = tree.parent() else {
        return Settled::Trusted;
    };
    if dir.join(DAMAGED).exists() {
        return Settled::Damaged;
    }
    // An unreadable `meta.json` is checked like an unflushed copy, never trusted: it can only
    // be unreadable when a crash interrupted the copy's writes.
    let meta = fs::read(dir.join(META))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Meta>(&bytes).ok());
    if meta.as_ref().is_some_and(|meta| trusted(dir, meta)) {
        return Settled::Trusted;
    }
    // A delta's rows live in it or in the full copy below it.
    let base = meta.as_ref().and_then(|meta| meta.base.clone());
    let locate = |relative: &Path| match &base {
        Some(base) => resolve_delta(tree, base, relative),
        None => tree.join(relative),
    };
    let intact = Manifest::open_file(&manifest_cache(tree)).is_ok_and(|rows| {
        meta.as_ref()
            .is_none_or(|meta| rows.len() as u64 == meta.entries)
            && rows.iter().all(|row| {
                manifest::decode_path(row.path).is_ok_and(|relative| {
                    Entry::lstat(&locate(&relative)).is_ok_and(|entry| {
                        manifest::ManifestKind::of_entry(&entry) == row.kind
                            && (row.kind == manifest::ManifestKind::Dir
                                || (entry.len == row.size
                                    && entry.mode == row.mode
                                    && (row.kind == manifest::ManifestKind::Symlink
                                        || entry.mtime_ns == row.mtime_ns)))
                    })
                })
            })
    });
    if intact && sync_filesystem(dir).is_ok() {
        let _ = fs::write(dir.join(DURABLE), b"");
        return Settled::Verified;
    }
    let _ = fs::write(dir.join(DAMAGED), b"");
    Settled::Damaged
}

const DELETED: &str = "deleted";

/// What [`freeze_delta`] made.
pub(crate) enum Delta {
    /// The project still matches the full copy: measure against it directly.
    Unchanged,
    /// The tree of a new delta.
    Made(PathBuf),
}

/// Record what `source` changed since the full frozen copy `base` as a delta beside it: the
/// changed and added entries (with the directories above them) in its tree, the deleted
/// paths in `deleted`, and the manifest of the whole of `source`. `pull` measures a
/// workspace against it without copying or linking every unchanged file, which a full copy
/// costs; [`resolve_delta`] finds each path's version. `None` when `base` has no manifest to
/// compare with (a copy older than manifests): the caller makes a full copy instead.
pub(crate) fn freeze_delta(source: &Path, base: &Path) -> Result<Option<Delta>> {
    let Ok(rows) = Manifest::open_file(&manifest_cache(base)) else {
        return Ok(None);
    };
    let Some(base_meta) = base
        .parent()
        .and_then(|dir| fs::read(dir.join(META)).ok())
        .and_then(|bytes| serde_json::from_slice::<Meta>(&bytes).ok())
        .filter(|meta| meta.base.is_none())
    else {
        return Ok(None);
    };
    let root = paths::frozen_root();
    ensure_private_dir(&root)?;
    let _lock = lock(&root, true)?;
    let source = source
        .canonicalize()
        .io_context(|| format!("canonicalize {}", source.display()))?;
    let started_ns = now_unix_ns();

    // One walk of the project: every entry's facts, and whether it differs from `base`.
    let walked = crate::util::walk::parallel(&source, |relative, path, entry| {
        let encoded = manifest::encode_path(relative);
        let differs = rows
            .get(&encoded)
            .is_none_or(|row| !same_as_row(path, entry, &row, base_meta.started_ns));
        Ok(Some((encoded, *entry, differs)))
    })?;
    let seen: std::collections::HashSet<&str> = walked
        .iter()
        .map(|(_, (encoded, _, _))| encoded.as_str())
        .collect();
    let deleted: Vec<&str> = rows
        .iter()
        .map(|row| row.path)
        .filter(|path| !seen.contains(path))
        .collect();
    if deleted.is_empty() && walked.iter().all(|(_, (_, _, differs))| !differs) {
        return Ok(Some(Delta::Unchanged));
    }

    // The delta's tree: what differs, and the directories that hold it.
    let mut wanted = std::collections::BTreeSet::new();
    for (relative, (_, _, differs)) in &walked {
        if *differs {
            wanted.extend(
                relative
                    .ancestors()
                    .filter(|path| !path.as_os_str().is_empty()),
            );
        }
    }
    let listed: Vec<(PathBuf, Entry, PathBuf)> = walked
        .iter()
        .filter(|(relative, _)| wanted.contains(relative.as_path()))
        .map(|(relative, (_, entry, _))| (relative.clone(), *entry, source.join(relative)))
        .collect();
    let mut manifest_rows = Vec::with_capacity(walked.len());
    for (relative, (encoded, entry, differs)) in &walked {
        manifest_rows.push(if *differs {
            manifest::row_of(relative, &source.join(relative), entry, u128::MAX)?
        } else {
            rows.get(encoded)
                .expect("an unchanged entry has a row")
                .to_row()
        });
    }

    let id = format!("{started_ns:x}-{}", std::process::id());
    let staging = root.join(format!(".{id}.tmp"));
    let tree = staging.join(TREE);
    let boot = boot_id();
    let built = (|| -> Result<u64> {
        fs::create_dir_all(&tree).io_context(|| format!("create {}", tree.display()))?;
        let (stats, _) =
            materialize_listed(listed, &tree, |_, _, _| Ok(Placed::No), boot.is_none())?;
        let mut deleted_list = deleted.join("\n");
        deleted_list.push('\n');
        fs::write(staging.join(DELETED), deleted_list)
            .io_context(|| format!("write {}", staging.display()))?;
        let temporary = staging.join(format!(".{MANIFEST}.tmp-{}", std::process::id()));
        manifest::write_file(
            &staging.join(MANIFEST),
            &temporary,
            manifest_rows,
            boot.is_none(),
        )?;
        let meta = Meta {
            source: source.clone(),
            started_ns,
            entries: walked.len() as u64,
            boot: boot.clone(),
            base: Some(base.to_path_buf()),
        };
        write_atomic(&staging.join(META), &serde_json::to_vec_pretty(&meta)?)?;
        Ok(stats.allocated_bytes)
    })();
    let allocated = match built {
        Ok(allocated) => allocated,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    let finished = root.join(&id);
    fs::rename(&staging, &finished)
        .io_context(|| format!("install frozen delta {}", finished.display()))?;
    sync_dir(&root)?;
    if boot.is_some() {
        PENDING_FLUSHES
            .lock()
            .expect("pending flushes")
            .push(finished.clone());
    }
    let own: u64 = [
        finished.join(META),
        finished.join(MANIFEST),
        finished.join(DELETED),
    ]
    .iter()
    .filter_map(|path| Entry::lstat(path).ok())
    .map(|entry| entry.blocks * 512)
    .sum();
    crate::disk::note_frozen_copy(&id, allocated + own);
    Ok(Some(Delta::Made(finished.join(TREE))))
}

/// Where the version of `relative` in the delta `delta` over the full copy `base` lives: the
/// delta's own entry, else `base`'s unless the delta records it deleted. The path may not
/// exist (no such version).
pub(crate) fn resolve_delta(delta: &Path, base: &Path, relative: &Path) -> PathBuf {
    let own = delta.join(relative);
    if fs::symlink_metadata(&own).is_ok() || deleted_in(delta, relative) {
        return own;
    }
    let below = base.join(relative);
    if fs::symlink_metadata(&below).is_ok() {
        below
    } else {
        own
    }
}

/// Whether the delta `delta` records `relative`, or a directory above it, as deleted.
fn deleted_in(delta: &Path, relative: &Path) -> bool {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, OnceLock};

    // A delta never changes, so its list is read once per process.
    static LISTS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<HashSet<String>>>>> =
        OnceLock::new();
    let list = {
        let mut lists = LISTS
            .get_or_init(Default::default)
            .lock()
            .expect("deleted lists");
        lists
            .entry(delta.to_path_buf())
            .or_insert_with(|| {
                let file = delta.parent().unwrap_or(delta).join(DELETED);
                Arc::new(
                    fs::read_to_string(file)
                        .unwrap_or_default()
                        .lines()
                        .filter(|line| !line.is_empty())
                        .map(str::to_string)
                        .collect(),
                )
            })
            .clone()
    };
    if list.is_empty() {
        return false;
    }
    let encoded = manifest::encode_path(relative);
    let mut prefix = encoded.as_str();
    loop {
        if list.contains(prefix) {
            return true;
        }
        match prefix.rfind('/') {
            Some(index) => prefix = &prefix[..index],
            None => return false,
        }
    }
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
            // A delta is not a whole tree: it can be measured against, never mounted.
            (meta.source == source
                && meta.base.is_none()
                && tree.is_dir()
                && trusted(&entry.path(), &meta))
            .then_some((tree, meta))
        })
        .max_by_key(|(_, meta)| meta.started_ns)
}

/// Whether `source` still matches the frozen copy `tree` entry for entry.
fn unchanged_since(source: &Path, tree: &Path, meta: &Meta) -> Result<bool> {
    // The copy's manifest describes every entry of it, so each project entry is compared with
    // its row instead of a second `lstat` in the copy. Older copies have no manifest.
    let rows = Manifest::open_file(&manifest_cache(tree)).ok();
    let differs = AtomicBool::new(false);
    let seen = AtomicU64::new(0);
    let walked = crate::util::walk::parallel::<(), _>(source, |relative, path, current| {
        seen.fetch_add(1, Ordering::Relaxed);
        if differs.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let same = match &rows {
            Some(rows) => rows
                .get(&manifest::encode_path(relative))
                .is_some_and(|row| same_as_row(path, current, &row, meta.started_ns)),
            None => Entry::lstat(&tree.join(relative)).is_ok_and(|frozen| {
                if current.is_dir() {
                    frozen.is_dir() && current.mode == frozen.mode
                } else if current.is_symlink() {
                    frozen.is_symlink()
                        && fs::read_link(path).ok() == fs::read_link(tree.join(relative)).ok()
                } else {
                    same_file(current, &frozen, meta.started_ns)
                }
            }),
        };
        if !same {
            differs.store(true, Ordering::Relaxed);
            // One difference settles it: stop the walk rather than read the rest.
            return Err(ForkspaceError::StateFailed(
                "the project changed".to_string(),
            ));
        }
        Ok(None)
    });
    if differs.load(Ordering::Relaxed) {
        return Ok(false);
    }
    walked?;
    Ok(seen.into_inner() == meta.entries)
}

/// [`same_file`] and its directory and symlink counterparts, against a manifest row of the
/// frozen copy instead of the copy itself.
fn same_as_row(path: &Path, current: &Entry, row: &manifest::RowRef<'_>, started_ns: u128) -> bool {
    use manifest::ManifestKind;

    if row.dirty {
        return false;
    }
    match row.kind {
        ManifestKind::Dir => current.is_dir() && current.mode == row.mode,
        ManifestKind::Symlink => {
            current.is_symlink()
                && fs::read_link(path)
                    .is_ok_and(|target| Some(manifest::encode_path(&target).as_str()) == row.target)
        }
        ManifestKind::File => {
            current.is_file()
                && current.ctime_ns < i128::try_from(started_ns).unwrap_or(i128::MAX)
                && current.len == row.size
                && current.mtime_ns == row.mtime_ns
                && current.mode == row.mode
        }
    }
}

/// Whether a project file (`current`) is still identical to its frozen copy (`frozen`): a
/// regular file with the same size, mtime, and mode, not changed since the copy was taken.
fn same_file(current: &Entry, frozen: &Entry, started_ns: u128) -> bool {
    current.is_file()
        && frozen.is_file()
        && current.ctime_ns < i128::try_from(started_ns).unwrap_or(i128::MAX)
        && current.len == frozen.len
        && current.mtime_ns == frozen.mtime_ns
        && current.mode == frozen.mode
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
