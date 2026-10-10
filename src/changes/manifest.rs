//! Fork manifests: a JSONL record of every entry in a snapshot view at fork time.
//!
//! Directory-snapshot backends have no upper dir, so the manifest is their change baseline:
//! one [`ManifestRow`] per entry, written to `<manifest_dir>/<name>.jsonl`. Paths are
//! percent-encoded ([`encode_path`]) so non-UTF-8 names survive the round trip. On macOS the
//! manifest is built in a detached background process ([`spawn`]).

mod worker;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Lines, Write};
use std::iter::Enumerate;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::backend::ChangeCount;
use crate::changes::filter::ChangeFilter;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::util::fs::{ctime_ns, mode_bits, mtime_ns};
use crate::util::{IoContext, WalkDirExt};
use crate::workspace::Workspace;

pub use worker::{run_worker_from_env, spawn};

/// Lifecycle of a workspace's fork manifest, persisted in `state.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ManifestStatus {
    #[default]
    Absent,
    Building,
    Ready,
    /// The manifest was lost after post-fork writes, so deletions can no longer be tracked.
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestKind {
    File,
    Dir,
    Symlink,
}

impl ManifestKind {
    /// The kind of entry described by `symlink_metadata` output.
    pub fn of(metadata: &fs::Metadata) -> Self {
        if metadata.file_type().is_symlink() {
            Self::Symlink
        } else if metadata.is_dir() {
            Self::Dir
        } else {
            Self::File
        }
    }
}

/// One manifest line: an entry's identity at fork time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRow {
    /// Path relative to the view, encoded with [`encode_path`].
    pub path: String,
    pub kind: ManifestKind,
    pub size: u64,
    pub mtime_ns: i128,
    /// Permission bits; `0` in manifests written before modes were recorded.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mode: u32,
    /// Encoded symlink target, for symlinks only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The entry was already written after the fork instant when the row was recorded, so
    /// the row may not describe the fork-time content.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dirty: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl ManifestRow {
    /// Whether the entry at `path`, with `metadata` from `symlink_metadata`, still looks like
    /// this row: same kind, size, mode (when recorded), symlink target, and mtime. Symlink
    /// mtimes are not compared because snapshot copies do not preserve them.
    pub(crate) fn matches(&self, path: &Path, metadata: &fs::Metadata) -> bool {
        if ManifestKind::of(metadata) != self.kind
            || metadata.len() != self.size
            || (self.kind != ManifestKind::Symlink && mtime_ns(metadata) != self.mtime_ns)
            || (self.mode != 0 && mode_bits(metadata) != self.mode)
        {
            return false;
        }
        if self.kind != ManifestKind::Symlink {
            return true;
        }
        fs::read_link(path)
            .ok()
            .is_some_and(|target| Some(encode_path(&target)) == self.target)
    }
}

/// Whether `metadata` shows a write after the fork instant (`ctime > fork_time_ns`).
pub(crate) fn written_after_fork(metadata: &fs::Metadata, fork_time_ns: u128) -> bool {
    ctime_ns(metadata) > i128::try_from(fork_time_ns).unwrap_or(i128::MAX)
}

pub fn manifest_path(name: &str) -> PathBuf {
    crate::paths::manifest_dir().join(format!("{name}.jsonl"))
}

/// Percent-encode every byte outside `[A-Za-z0-9/._-]`, keeping the path lossless and the
/// JSON plain ASCII.
pub fn encode_path(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    let mut encoded = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    encoded
}

/// Inverse of [`encode_path`]; accepts either hex case.
pub fn decode_path(encoded: &str) -> Result<PathBuf> {
    let invalid_escape =
        || ForkspaceError::StateCorrupted(format!("invalid manifest path escape in '{encoded}'"));
    let mut bytes = encoded.bytes();
    let mut decoded = Vec::with_capacity(encoded.len());
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            decoded.push(byte);
            continue;
        }
        let high = bytes.next().ok_or_else(invalid_escape)?;
        let low = bytes.next().ok_or_else(invalid_escape)?;
        let value = hex_value(high)
            .zip(hex_value(low))
            .map(|(high, low)| (high << 4) | low)
            .ok_or_else(invalid_escape)?;
        decoded.push(value);
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

fn hex_value(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

fn row(root: &Path, entry: &walkdir::DirEntry, fork_time_ns: u128) -> Result<ManifestRow> {
    let path = entry.path();
    let metadata = fs::symlink_metadata(path)
        .io_context(|| format!("read manifest metadata for {}", path.display()))?;
    let relative = path.strip_prefix(root).map_err(|_| {
        ForkspaceError::StateFailed(format!(
            "manifest entry {} escaped workspace {}",
            path.display(),
            root.display()
        ))
    })?;
    row_of(relative, path, &metadata, fork_time_ns)
}

/// The manifest row of the entry at `path` (`relative` to the view) with `lstat` `metadata`.
fn row_of(
    relative: &Path,
    path: &Path,
    metadata: &fs::Metadata,
    fork_time_ns: u128,
) -> Result<ManifestRow> {
    let metadata = metadata.clone();
    let kind = ManifestKind::of(&metadata);
    let target = if kind == ManifestKind::Symlink {
        let target =
            fs::read_link(path).io_context(|| format!("read symlink {}", path.display()))?;
        Some(encode_path(&target))
    } else {
        None
    };
    Ok(ManifestRow {
        path: encode_path(relative),
        kind,
        size: metadata.len(),
        mtime_ns: mtime_ns(&metadata),
        mode: mode_bits(&metadata),
        target,
        dirty: written_after_fork(&metadata, fork_time_ns),
    })
}

/// Walk `view` and durably install its manifest for workspace `name`. Returns the row count.
pub fn build(name: &str, view: &Path, fork_time_ns: u128) -> Result<usize> {
    build_rows(name, view, None, fork_time_ns)
}

/// [`build`] for a manifest recorded after the workspace was handed to the user (the macOS
/// background builder). Entries deleted or renamed in the view before the walk reached them
/// are recovered from `base`: wherever a view directory changed after the fork, base entries
/// it no longer holds are recorded from the base, as long as the base has not changed them
/// since the fork either. Without this, such deletions would never show up in `diff`.
pub fn build_after_fork(name: &str, view: &Path, base: &Path, fork_time_ns: u128) -> Result<usize> {
    build_rows(name, view, Some(base), fork_time_ns)
}

fn build_rows(name: &str, view: &Path, base: Option<&Path>, fork_time_ns: u128) -> Result<usize> {
    let path = manifest_path(name);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)
        .io_context(|| format!("create manifest directory {}", directory.display()))?;
    // State recovery deletes `.<name>.jsonl.tmp-<pid>` leftovers once `<pid>` has exited, so
    // an interrupted build is cleaned up later. Keep this exact name.
    let temporary = directory.join(format!(".{name}.jsonl.tmp-{}", std::process::id()));
    let file = fs::File::create(&temporary)
        .io_context(|| format!("create manifest {}", temporary.display()))?;

    let mut writer = BufWriter::new(file);
    let count = write_rows(&mut writer, view, base, fork_time_ns, &temporary)?;
    let file = writer.into_inner().map_err(|error| ForkspaceError::Io {
        context: format!("flush manifest {}", temporary.display()),
        source: error.into_error(),
    })?;

    file.sync_all()
        .io_context(|| format!("sync manifest {}", temporary.display()))?;
    fs::rename(&temporary, &path).io_context(|| format!("install manifest {}", path.display()))?;
    fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .io_context(|| format!("sync manifest directory {}", directory.display()))?;
    Ok(count)
}

fn write_rows(
    writer: &mut impl Write,
    view: &Path,
    base: Option<&Path>,
    fork_time_ns: u128,
    temporary: &Path,
) -> Result<usize> {
    let mut write = |root: &Path, entry: &walkdir::DirEntry| -> Result {
        serde_json::to_writer(&mut *writer, &row(root, entry, fork_time_ns)?)?;
        writer
            .write_all(b"\n")
            .io_context(|| format!("write manifest {}", temporary.display()))
    };
    let Some(base) = base else {
        // At fork time the view is not in use yet: read it on several threads.
        let rows = crate::util::walk::parallel(view, |relative, path, metadata| {
            row_of(relative, path, metadata, fork_time_ns).map(Some)
        })?;
        for (_, row) in &rows {
            serde_json::to_writer(&mut *writer, row)?;
            writer
                .write_all(b"\n")
                .io_context(|| format!("write manifest {}", temporary.display()))?;
        }
        return Ok(rows.len());
    };
    let mut count = 0;
    for entry in WalkDir::new(view).follow_links(false) {
        let entry = entry.walk_context(view)?;
        if entry.depth() > 0 {
            write(view, &entry)?;
            count += 1;
        }
        let changed_dir = entry.file_type().is_dir()
            && entry
                .metadata()
                .is_ok_and(|metadata| written_after_fork(&metadata, fork_time_ns));
        if changed_dir {
            let relative = entry.path().strip_prefix(view).unwrap_or(Path::new(""));
            for missing in entries_missing_from_view(view, base, relative, fork_time_ns)? {
                write(base, &missing)?;
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Base entries under directory `relative` that the view no longer has and that the base has
/// not changed since `fork_time_ns`, with their unchanged descendants.
fn entries_missing_from_view(
    view: &Path,
    base: &Path,
    relative: &Path,
    fork_time_ns: u128,
) -> Result<Vec<walkdir::DirEntry>> {
    let base_dir = base.join(relative);
    let Ok(children) = fs::read_dir(&base_dir) else {
        return Ok(Vec::new());
    };
    let mut missing = Vec::new();
    for child in children.flatten() {
        if fs::symlink_metadata(view.join(relative).join(child.file_name())).is_ok() {
            continue;
        }
        let unchanged_since_fork = |entry: &walkdir::DirEntry| {
            entry
                .metadata()
                .is_ok_and(|metadata| !written_after_fork(&metadata, fork_time_ns))
        };
        for entry in WalkDir::new(child.path())
            .follow_links(false)
            .into_iter()
            .filter_entry(unchanged_since_fork)
        {
            missing.push(entry.walk_context(&base_dir)?);
        }
    }
    Ok(missing)
}

/// Streaming reader over a manifest's rows; see [`stream`].
pub struct ManifestReader {
    path: PathBuf,
    lines: Enumerate<Lines<BufReader<fs::File>>>,
}

impl Iterator for ManifestReader {
    type Item = Result<ManifestRow>;

    fn next(&mut self) -> Option<Self::Item> {
        let (index, line) = self.lines.next()?;
        let line_number = index + 1;
        let path = &self.path;
        let row = line
            .io_context(|| format!("read manifest {} line {line_number}", path.display()))
            .and_then(|line| {
                serde_json::from_str(&line).map_err(|error| {
                    ForkspaceError::StateCorrupted(format!(
                        "invalid manifest {} line {line_number}: {error}",
                        path.display()
                    ))
                })
            });
        Some(row)
    }
}

/// Open workspace `name`'s manifest for row-by-row reading.
pub fn stream(name: &str) -> Result<ManifestReader> {
    let path = manifest_path(name);
    let file = fs::File::open(&path).io_context(|| format!("open manifest {}", path.display()))?;
    Ok(ManifestReader {
        path,
        lines: BufReader::new(file).lines().enumerate(),
    })
}

pub fn read(name: &str) -> Result<Vec<ManifestRow>> {
    stream(name)?.collect()
}

/// Delete workspace `name`'s manifest; a missing manifest is not an error.
pub fn remove(name: &str) -> Result {
    let path = manifest_path(name);
    match fs::remove_file(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result.io_context(|| format!("remove manifest {}", path.display())),
    }
}

/// Persist a new manifest status. Returns `false` if the workspace no longer exists.
fn mark(name: &str, status: ManifestStatus) -> Result<bool> {
    State::with_state(|state| {
        let Some(workspace) = state.workspaces.get_mut(name) else {
            return Ok(false);
        };
        workspace.manifest_status = status;
        Ok(true)
    })
}

/// Count changed non-directory paths in a snapshot view by comparing a fresh walk against
/// the manifest. An unreadable manifest triggers recovery (see [`recover_unreadable`]).
pub fn change_count(workspace: &Workspace, include_ignored: bool) -> Result<ChangeCount> {
    match workspace.manifest_status {
        ManifestStatus::Absent | ManifestStatus::Building => return Ok(ChangeCount::Pending),
        ManifestStatus::Degraded => return Ok(ChangeCount::Degraded),
        ManifestStatus::Ready => {}
    }

    let Ok(rows) = read(&workspace.name) else {
        return recover_unreadable(workspace);
    };
    let mut baseline = BTreeMap::new();
    for row in rows {
        baseline.insert(decode_path(&row.path)?, row);
    }
    let current = current_rows(workspace)?;
    let filter = (!include_ignored).then(|| ChangeFilter::build(&workspace.base));
    let counted = |path: &Path, row: &ManifestRow| {
        row.kind != ManifestKind::Dir
            && filter
                .as_ref()
                .is_none_or(|filter| filter.is_visible(path, false))
    };

    let added_or_modified = current
        .iter()
        .filter(|(path, row)| counted(path, row))
        .filter(|(path, row)| {
            baseline
                .get(*path)
                .is_none_or(|base| base != *row || base.dirty)
        })
        .count();
    let deleted = baseline
        .iter()
        .filter(|(path, row)| counted(path, row) && !current.contains_key(*path))
        .count();
    Ok(ChangeCount::Ready(added_or_modified + deleted))
}

/// A `Ready` manifest that cannot be read. If the view has been written since the fork, the
/// fork-time baseline is gone for good and tracking degrades; otherwise the view still *is*
/// the baseline, so the manifest is rebuilt in the background.
fn recover_unreadable(workspace: &Workspace) -> Result<ChangeCount> {
    if has_post_fork_writes(workspace)? {
        let _ = mark(&workspace.name, ManifestStatus::Degraded);
        return Ok(ChangeCount::Degraded);
    }
    let _ = mark(&workspace.name, ManifestStatus::Building);
    let _ = remove(&workspace.name);
    let _ = spawn(
        workspace.name.clone(),
        workspace.view_path.clone(),
        workspace.base.clone(),
        workspace.fork_time_ns,
    );
    Ok(ChangeCount::Pending)
}

/// Manifest rows describing the view as it is now, keyed by decoded path.
fn current_rows(workspace: &Workspace) -> Result<BTreeMap<PathBuf, ManifestRow>> {
    let view = &workspace.view_path;
    let mut rows = BTreeMap::new();
    for entry in WalkDir::new(view).min_depth(1).follow_links(false) {
        let entry = entry.walk_context(view)?;
        let row = row(view, &entry, workspace.fork_time_ns)?;
        rows.insert(decode_path(&row.path)?, row);
    }
    Ok(rows)
}

fn has_post_fork_writes(workspace: &Workspace) -> Result<bool> {
    let view = &workspace.view_path;
    for entry in WalkDir::new(view).follow_links(false) {
        let entry = entry.walk_context(view)?;
        let metadata = fs::symlink_metadata(entry.path())
            .io_context(|| format!("inspect {} for manifest recovery", entry.path().display()))?;
        if written_after_fork(&metadata, workspace.fork_time_ns) {
            return Ok(true);
        }
    }
    Ok(false)
}
