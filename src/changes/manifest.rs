//! Fork manifests: a JSONL record of every entry in a snapshot view at fork time.
//!
//! Directory-snapshot backends have no upper dir, so the manifest is their change baseline:
//! one [`ManifestRow`] per entry, written to `<manifest_dir>/<name>.jsonl`. Paths are
//! percent-encoded ([`encode_path`]) so non-UTF-8 names survive the round trip. On macOS the
//! manifest is built in a detached background process ([`spawn`]).
//!
//! The file is binary despite its name: a header, an offset per row, and the rows sorted by
//! encoded path, so [`Manifest`] finds a row by binary search and reads every row without
//! parsing text. Manifests written before this format are JSON lines; they are still read.

mod worker;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufWriter, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::backend::ChangeCount;
use crate::changes::filter::ChangeFilter;
use crate::error::{ForkspaceError, Result};
use crate::state::State;
use crate::util::walk::{Entry, Kind};
use crate::util::{IoContext, WalkDirExt};
use crate::workspace::Workspace;

pub(crate) use worker::can_spawn;
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

/// The process building a workspace's manifest in the background. A `building` status whose
/// builder is gone is restarted (STATE-02): by the next command on macOS, and by any command
/// that needs the manifest everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Builder {
    pub pid: u32,
    /// Guards against PID reuse; see [`crate::process::watch`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(flatten)]
    pub unknown_fields: serde_json::Map<String, serde_json::Value>,
}

impl Builder {
    /// The running process `pid`.
    pub fn of(pid: u32) -> Self {
        Self {
            pid,
            identity: crate::process::watch::identity(pid),
            unknown_fields: serde_json::Map::new(),
        }
    }

    /// Whether this exact process (not a PID recycler) still runs.
    pub fn is_alive(&self) -> bool {
        crate::process::watch::is_alive(self.pid, self.identity.as_deref())
    }
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

    /// The kind of a walked entry.
    pub fn of_entry(entry: &Entry) -> Self {
        match entry.kind {
            Kind::Symlink => Self::Symlink,
            Kind::Dir => Self::Dir,
            Kind::File | Kind::Other => Self::File,
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
    /// Whether the entry at `path`, with `lstat` facts `entry`, still looks like this row:
    /// same kind, size, mode (when recorded), symlink target, and mtime. Symlink mtimes are
    /// not compared because snapshot copies do not preserve them.
    pub(crate) fn matches(&self, path: &Path, entry: &Entry) -> bool {
        if ManifestKind::of_entry(entry) != self.kind
            || entry.len != self.size
            || (self.kind != ManifestKind::Symlink && entry.mtime_ns != self.mtime_ns)
            || (self.mode != 0 && entry.mode != self.mode)
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

/// Whether `entry` shows a write after the fork instant (`ctime > fork_time_ns`).
pub(crate) fn written_after_fork(entry: &Entry, fork_time_ns: u128) -> bool {
    entry.ctime_ns > i128::try_from(fork_time_ns).unwrap_or(i128::MAX)
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
    row_of(relative, path, &Entry::of(&metadata), fork_time_ns)
}

/// The manifest row of the entry at `path` (`relative` to the view) with facts `entry`.
pub(crate) fn row_of(
    relative: &Path,
    path: &Path,
    entry: &Entry,
    fork_time_ns: u128,
) -> Result<ManifestRow> {
    let kind = ManifestKind::of_entry(entry);
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
        size: entry.len,
        mtime_ns: entry.mtime_ns,
        mode: entry.mode,
        target,
        dirty: written_after_fork(entry, fork_time_ns),
    })
}

/// Walk `view` and durably install its manifest for workspace `name`. Returns the row count.
pub fn build(name: &str, view: &Path, fork_time_ns: u128) -> Result<usize> {
    let rows = fork_rows(view, fork_time_ns)?;
    install_rows(name, rows)
}

/// The rows [`build`] would record for `view`, without installing them.
pub(crate) fn rows_of_tree(view: &Path, fork_time_ns: u128) -> Result<Vec<ManifestRow>> {
    fork_rows(view, fork_time_ns)
}

/// Durably install `rows` as the manifest of workspace `name`. Returns the row count.
pub(crate) fn install_rows(name: &str, rows: Vec<ManifestRow>) -> Result<usize> {
    write_file(&manifest_path(name), &temporary_path(name), rows, true)
}

/// [`build`] from entries a copy recorded while it created `view` (see
/// `materialize_recorded`), instead of walking `view`. Nothing was written after the fork
/// yet, so no row is dirty.
pub(crate) fn build_from_entries(
    name: &str,
    view: &Path,
    entries: &[(PathBuf, Entry)],
) -> Result<usize> {
    let rows = entries
        .iter()
        .map(|(relative, entry)| row_of(relative, &view.join(relative), entry, u128::MAX))
        .collect::<Result<Vec<_>>>()?;
    write_file(&manifest_path(name), &temporary_path(name), rows, true)
}

/// [`build`] for a manifest recorded after the workspace was handed to the user (the macOS
/// background builder). Entries deleted or renamed in the view before the walk reached them
/// are recovered from `base`: wherever a view directory changed after the fork, base entries
/// it no longer holds are recorded from the base, as long as the base has not changed them
/// since the fork either. Without this, such deletions would never show up in `diff`.
pub fn build_after_fork(name: &str, view: &Path, base: &Path, fork_time_ns: u128) -> Result<usize> {
    let path = manifest_path(name);
    let rows = late_rows(view, base, None, fork_time_ns)?;
    write_file(&path, &temporary_path(name), rows, true)
}

/// Where this process writes workspace `name`'s manifest before installing it. State recovery
/// deletes `.<name>.jsonl.tmp-<pid>` leftovers once `<pid>` has exited, so an interrupted build
/// is cleaned up later. Keep this exact name.
fn temporary_path(name: &str) -> PathBuf {
    crate::paths::manifest_dir().join(format!(".{name}.jsonl.tmp-{}", std::process::id()))
}

/// Install `rows` as the binary manifest at `path`, written first to `temporary` in the same
/// directory. With `durable`, the file and the rename are synced before this returns; without,
/// the caller flushes them later. Returns the row count.
pub(crate) fn write_file(
    path: &Path,
    temporary: &Path,
    rows: Vec<ManifestRow>,
    durable: bool,
) -> Result<usize> {
    let count = write_temporary(temporary, rows, durable)?;
    fs::rename(temporary, path).io_context(|| format!("install manifest {}", path.display()))?;
    if durable {
        crate::util::fs::sync_dir(path.parent().unwrap_or_else(|| Path::new(".")))?;
    }
    Ok(count)
}

/// Write `rows` as a binary manifest to `temporary`, synced with `durable`.
fn write_temporary(temporary: &Path, mut rows: Vec<ManifestRow>, durable: bool) -> Result<usize> {
    let directory = temporary.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)
        .io_context(|| format!("create manifest directory {}", directory.display()))?;
    let file = fs::File::create(temporary)
        .io_context(|| format!("create manifest {}", temporary.display()))?;
    let mut writer = BufWriter::new(file);
    write_binary(&mut writer, &mut rows)
        .io_context(|| format!("write manifest {}", temporary.display()))?;
    let file = writer.into_inner().map_err(|error| ForkspaceError::Io {
        context: format!("flush manifest {}", temporary.display()),
        source: error.into_error(),
    })?;
    if durable {
        file.sync_all()
            .io_context(|| format!("sync manifest {}", temporary.display()))?;
    }
    Ok(rows.len())
}

/// The rows of every entry in `view` at fork time, when nobody uses it yet: read on several
/// threads.
fn fork_rows(view: &Path, fork_time_ns: u128) -> Result<Vec<ManifestRow>> {
    let rows = crate::util::walk::parallel(view, |relative, path, metadata| {
        row_of(relative, path, metadata, fork_time_ns).map(Some)
    })?;
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

/// The rows of every entry in `view`, walked after the fork while it may be in use, with the
/// entries the view lost since the fork recovered from `base` and, when given, the fork-time
/// copy `fork_copy` (see [`build_after_fork`]). An entry deleted while the walk reads it is
/// recovered the same way instead of failing the build.
fn late_rows(
    view: &Path,
    base: &Path,
    fork_copy: Option<&Path>,
    fork_time_ns: u128,
) -> Result<Vec<ManifestRow>> {
    let mut rows = Vec::new();
    let mut walker = WalkDir::new(view).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = match entry {
            Err(error) if vanished(error.io_error()) => {
                let lost = error.path().and_then(|path| path.strip_prefix(view).ok());
                if let Some((parent, name)) =
                    lost.and_then(|lost| lost.parent().zip(lost.file_name()))
                {
                    rows.extend(lost_entries(
                        view,
                        base,
                        fork_copy,
                        parent,
                        Some(name),
                        fork_time_ns,
                    )?);
                }
                continue;
            }
            entry => entry.walk_context(view)?,
        };
        let relative = entry.path().strip_prefix(view).unwrap_or(Path::new(""));
        if entry.depth() > 0 {
            match row(view, &entry, fork_time_ns) {
                Ok(row) => rows.push(row),
                Err(ForkspaceError::Io { source, .. }) if vanished(Some(&source)) => {
                    if entry.file_type().is_dir() {
                        walker.skip_current_dir();
                    }
                    if let Some((parent, name)) = relative.parent().zip(relative.file_name()) {
                        rows.extend(lost_entries(
                            view,
                            base,
                            fork_copy,
                            parent,
                            Some(name),
                            fork_time_ns,
                        )?);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        let changed_dir = entry.file_type().is_dir()
            && entry
                .metadata()
                .is_ok_and(|metadata| written_after_fork(&Entry::of(&metadata), fork_time_ns));
        if changed_dir {
            rows.extend(lost_entries(
                view,
                base,
                fork_copy,
                relative,
                None,
                fork_time_ns,
            )?);
        }
    }
    // An entry deleted while being read can be recorded from the view and recovered too.
    rows.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    rows.dedup_by(|later, earlier| later.path == earlier.path);
    Ok(rows)
}

/// Whether an I/O error says the entry is gone.
fn vanished(error: Option<&std::io::Error>) -> bool {
    error.is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// Rows for the entries under directory `relative` (only its child `only`, when given) that
/// the view no longer has: from `base` where the base has not changed them since the fork,
/// with their unchanged descendants, then the files and symlinks of the fork-time copy
/// `fork_copy` the base could not give. Recovery is best effort: an entry that cannot be read
/// is left out rather than failing the build.
fn lost_entries(
    view: &Path,
    base: &Path,
    fork_copy: Option<&Path>,
    relative: &Path,
    only: Option<&std::ffi::OsStr>,
    fork_time_ns: u128,
) -> Result<Vec<ManifestRow>> {
    let view_dir = view.join(relative);
    let lost = |name: &std::ffi::OsStr| {
        only.is_none_or(|only| only == name) && fs::symlink_metadata(view_dir.join(name)).is_err()
    };
    let mut rows = Vec::new();
    let mut recovered = std::collections::HashSet::new();
    for (root, from_copy) in [(Some(base), false), (fork_copy, true)] {
        let Some(root) = root else {
            continue;
        };
        let Ok(children) = fs::read_dir(root.join(relative)) else {
            continue;
        };
        for child in children.flatten() {
            if !lost(&child.file_name()) {
                continue;
            }
            let unchanged_since_fork = |entry: &walkdir::DirEntry| {
                from_copy
                    || entry.metadata().is_ok_and(|metadata| {
                        !written_after_fork(&Entry::of(&metadata), fork_time_ns)
                    })
            };
            let walker = WalkDir::new(child.path())
                .follow_links(false)
                // A lost symlink is recorded as itself, never through its target.
                .follow_root_links(false)
                .into_iter()
                .filter_entry(unchanged_since_fork);
            for entry in walker.flatten() {
                // The copy's directories are made by the copy, not taken from the fork.
                if from_copy && entry.file_type().is_dir() {
                    continue;
                }
                let Ok(mut row) = row(root, &entry, fork_time_ns) else {
                    continue;
                };
                if from_copy {
                    if recovered.contains(&row.path) {
                        continue;
                    }
                    // The copy was taken after the fork instant; its content is the fork's.
                    row.dirty = false;
                } else {
                    recovered.insert(row.path.clone());
                }
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

/// The rows of a workspace that was pulled: its baseline is the base as the pull left it, so
/// the view's own entries say nothing about it. The fork-time copy (taken by the pull) gives
/// files and symlinks; the base gives the rest, where nothing wrote it since the pull, and its
/// directories (dirty when written since). An entry known to neither has no row, which merge
/// and pull treat as unknown rather than unchanged.
fn pulled_rows(base: &Path, fork_copy: Option<&Path>, fork_time_ns: u128) -> Vec<ManifestRow> {
    let mut rows = Vec::new();
    let mut known = std::collections::HashSet::new();
    if let Some(copy) = fork_copy {
        let entries = WalkDir::new(copy).min_depth(1).follow_links(false);
        for entry in entries.into_iter().flatten() {
            if entry.file_type().is_dir() {
                continue;
            }
            if let Ok(mut row) = row(copy, &entry, fork_time_ns) {
                row.dirty = false;
                known.insert(row.path.clone());
                rows.push(row);
            }
        }
    }
    for entry in WalkDir::new(base)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        let Ok(row) = row(base, &entry, fork_time_ns) else {
            continue;
        };
        if known.contains(&row.path) || (row.dirty && row.kind != ManifestKind::Dir) {
            continue;
        }
        rows.push(row);
    }
    rows
}

/// First bytes of a binary manifest: magic, row count, and a checksum of everything after the
/// header, each a little-endian `u64`; one `u64` offset per row follows. Manifests without a
/// magic are JSON lines (the format before), which [`Manifest::open_file`] still reads.
const MAGIC: &[u8; 8] = b"FSMANIF2";
const HEADER: usize = 24;
/// The first binary format: no checksum, so every row is parsed to check the file.
const MAGIC_V1: &[u8; 8] = b"FSMANIF1";
const HEADER_V1: usize = 16;
const DIRTY: u8 = 1;
const HAS_TARGET: u8 = 2;

/// A fork manifest loaded for lookups, its rows sorted by encoded path.
pub struct Manifest {
    rows: Rows,
}

enum Rows {
    /// A binary manifest checked when it was opened; its offset table starts at `table`.
    Binary {
        bytes: Vec<u8>,
        count: usize,
        table: usize,
    },
    /// A JSON-lines manifest, parsed and sorted.
    Parsed(Vec<ManifestRow>),
}

/// One row of a [`Manifest`], borrowed from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowRef<'a> {
    pub path: &'a str,
    pub kind: ManifestKind,
    pub size: u64,
    pub mtime_ns: i128,
    pub mode: u32,
    pub target: Option<&'a str>,
    pub dirty: bool,
}

impl RowRef<'_> {
    pub fn to_row(&self) -> ManifestRow {
        ManifestRow {
            path: self.path.to_string(),
            kind: self.kind,
            size: self.size,
            mtime_ns: self.mtime_ns,
            mode: self.mode,
            target: self.target.map(str::to_string),
            dirty: self.dirty,
        }
    }
}

impl<'a> From<&'a ManifestRow> for RowRef<'a> {
    fn from(row: &'a ManifestRow) -> Self {
        Self {
            path: &row.path,
            kind: row.kind,
            size: row.size,
            mtime_ns: row.mtime_ns,
            mode: row.mode,
            target: row.target.as_deref(),
            dirty: row.dirty,
        }
    }
}

impl Manifest {
    /// Workspace `name`'s manifest.
    pub fn open(name: &str) -> Result<Self> {
        Self::open_file(&manifest_path(name))
    }

    /// The manifest file at `path`, in either format.
    pub fn open_file(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).io_context(|| format!("open manifest {}", path.display()))?;
        let invalid =
            || ForkspaceError::StateCorrupted(format!("invalid manifest {}", path.display()));
        if bytes.starts_with(MAGIC) {
            let count = summed_count(&bytes).ok_or_else(invalid)?;
            return Ok(Self {
                rows: Rows::Binary {
                    bytes,
                    count,
                    table: HEADER,
                },
            });
        }
        if bytes.starts_with(MAGIC_V1) {
            let count = checked_count(&bytes, HEADER_V1).ok_or_else(invalid)?;
            return Ok(Self {
                rows: Rows::Binary {
                    bytes,
                    count,
                    table: HEADER_V1,
                },
            });
        }
        let mut rows = Vec::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let row: ManifestRow = serde_json::from_slice(line).map_err(|error| {
                ForkspaceError::StateCorrupted(format!(
                    "invalid manifest {} line {}: {error}",
                    path.display(),
                    index + 1
                ))
            })?;
            rows.push(row);
        }
        rows.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        Ok(Self {
            rows: Rows::Parsed(rows),
        })
    }

    pub fn len(&self) -> usize {
        match &self.rows {
            Rows::Binary { count, .. } => *count,
            Rows::Parsed(rows) => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The row at `index` in path order.
    pub fn row(&self, index: usize) -> RowRef<'_> {
        match &self.rows {
            Rows::Binary { bytes, table, .. } => {
                binary_row(bytes, *table, index)
                    .expect("manifest rows were checked when it was opened")
                    .0
            }
            Rows::Parsed(rows) => RowRef::from(&rows[index]),
        }
    }

    /// Every row, in path order.
    pub fn iter(&self) -> impl Iterator<Item = RowRef<'_>> {
        (0..self.len()).map(|index| self.row(index))
    }

    /// The row whose encoded path is `encoded`.
    pub fn get(&self, encoded: &str) -> Option<RowRef<'_>> {
        self.position(encoded).map(|index| self.row(index))
    }

    /// The index of the row whose encoded path is `encoded`.
    pub fn position(&self, encoded: &str) -> Option<usize> {
        let index = self.lower_bound(encoded);
        (index < self.len() && self.path_at(index) == encoded).then_some(index)
    }

    /// The encoded path of row `index`, read without the rest of the row.
    pub fn path_at(&self, index: usize) -> &str {
        match &self.rows {
            Rows::Binary { bytes, table, .. } => binary_path(bytes, *table, index)
                .expect("manifest rows were checked when it was opened"),
            Rows::Parsed(rows) => &rows[index].path,
        }
    }

    /// The rows below the directory whose encoded path is `encoded`, in path order.
    pub fn under<'a>(&'a self, encoded: &str) -> impl Iterator<Item = RowRef<'a>> + 'a {
        let prefix = format!("{encoded}/");
        let start = self.lower_bound(&prefix);
        (start..self.len())
            .map(|index| self.row(index))
            .take_while(move |row| row.path.starts_with(&prefix))
    }

    /// The index just past the rows below the directory `encoded`: `'0'` follows `'/'` in
    /// byte order, so `dir0` is the first path after every `dir/...`.
    pub fn end_of_subtree(&self, encoded: &str) -> usize {
        self.lower_bound(&format!("{encoded}0"))
    }

    /// The index of the first row whose path is not below `encoded` in byte order.
    pub fn lower_bound(&self, encoded: &str) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = (low + high) / 2;
            if self.path_at(middle).as_bytes() < encoded.as_bytes() {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }
}

/// The row count of a checksummed manifest whose checksum matches, after checking every
/// offset is in bounds, or `None` if the file is damaged. A matching checksum means the rows
/// are the bytes that were written, so they are not parsed here.
fn summed_count(bytes: &[u8]) -> Option<usize> {
    let count = usize::try_from(read_u64(bytes, MAGIC.len())?).ok()?;
    let table_end = HEADER.checked_add(count.checked_mul(8)?)?;
    if table_end > bytes.len() || read_u64(bytes, 16)? != checksum(&bytes[HEADER..]) {
        return None;
    }
    let mut previous = table_end;
    for index in 0..count {
        let offset = usize::try_from(read_u64(bytes, HEADER + index * 8)?).ok()?;
        if offset < previous || offset >= bytes.len() {
            return None;
        }
        previous = offset;
    }
    // The last row must end exactly at the end of the file.
    if count > 0 {
        let (_, end) = binary_row(bytes, HEADER, count - 1)?;
        if end != bytes.len() {
            return None;
        }
    } else if table_end != bytes.len() {
        return None;
    }
    Some(count)
}

/// A fast 64-bit checksum of `bytes` (a multiply-rotate mix of 8-byte words); it catches
/// torn, truncated and appended files, not tampering.
fn checksum(bytes: &[u8]) -> u64 {
    const MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut hash = MULTIPLIER ^ bytes.len() as u64;
    let (words, rest) = bytes.as_chunks::<8>();
    for word in words {
        hash = (hash ^ u64::from_le_bytes(*word))
            .wrapping_mul(MULTIPLIER)
            .rotate_left(31);
    }
    for &byte in rest {
        hash = (hash ^ u64::from(byte))
            .wrapping_mul(MULTIPLIER)
            .rotate_left(31);
    }
    hash
}

/// The row count of a first-format binary manifest after checking that every offset points
/// at a whole row, or `None` if the file is damaged.
fn checked_count(bytes: &[u8], header: usize) -> Option<usize> {
    let count = usize::try_from(read_u64(bytes, MAGIC.len())?).ok()?;
    let table_end = header.checked_add(count.checked_mul(8)?)?;
    if table_end > bytes.len() {
        return None;
    }
    let mut previous: Option<&str> = None;
    let mut end = table_end;
    for index in 0..count {
        let (row, row_end) = binary_row(bytes, header, index)?;
        if previous.is_some_and(|previous| previous.as_bytes() > row.path.as_bytes()) {
            return None;
        }
        previous = Some(row.path);
        end = end.max(row_end);
    }
    // Anything after the last row (an append, a second write) means the file is not the one
    // that was written.
    (end == bytes.len()).then_some(count)
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// The path of row `index` of a binary manifest whose offset table starts at `table`.
fn binary_path(bytes: &[u8], table: usize, index: usize) -> Option<&str> {
    let offset = usize::try_from(read_u64(bytes, table + index * 8)?).ok()?;
    let len = read_u32(bytes, offset)? as usize;
    let start = offset + 4;
    std::str::from_utf8(bytes.get(start..start.checked_add(len)?)?).ok()
}

/// Row `index` of a binary manifest whose offset table starts at `table`, and the offset
/// just past it.
fn binary_row(bytes: &[u8], table: usize, index: usize) -> Option<(RowRef<'_>, usize)> {
    let offset = usize::try_from(read_u64(bytes, table + index * 8)?).ok()?;
    let mut at = offset;
    let text = |at: &mut usize| -> Option<&str> {
        let len = read_u32(bytes, *at)? as usize;
        let start = *at + 4;
        let value = std::str::from_utf8(bytes.get(start..start.checked_add(len)?)?).ok()?;
        *at = start + len;
        Some(value)
    };
    let path = text(&mut at)?;
    let kind = match *bytes.get(at)? {
        0 => ManifestKind::File,
        1 => ManifestKind::Dir,
        2 => ManifestKind::Symlink,
        _ => return None,
    };
    let flags = *bytes.get(at + 1)?;
    let mode = read_u32(bytes, at + 2)?;
    let size = read_u64(bytes, at + 6)?;
    let mtime_ns = i128::from_le_bytes(bytes.get(at + 14..at + 30)?.try_into().ok()?);
    at += 30;
    let target = if flags & HAS_TARGET != 0 {
        Some(text(&mut at)?)
    } else {
        None
    };
    let row = RowRef {
        path,
        kind,
        size,
        mtime_ns,
        mode,
        target,
        dirty: flags & DIRTY != 0,
    };
    Some((row, at))
}

/// Write `rows` as a binary manifest, sorted by path.
pub(crate) fn write_binary(
    writer: &mut impl Write,
    rows: &mut [ManifestRow],
) -> std::io::Result<()> {
    rows.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let row_len = |row: &ManifestRow| {
        4 + row.path.len() + 30 + row.target.as_ref().map_or(0, |target| 4 + target.len())
    };
    let mut body = Vec::with_capacity(rows.iter().map(|row| 8 + row_len(row)).sum());
    let mut offset = (HEADER + rows.len() * 8) as u64;
    for row in rows.iter() {
        body.extend_from_slice(&offset.to_le_bytes());
        offset += row_len(row) as u64;
    }
    for row in rows.iter() {
        body.extend_from_slice(&(row.path.len() as u32).to_le_bytes());
        body.extend_from_slice(row.path.as_bytes());
        body.push(match row.kind {
            ManifestKind::File => 0,
            ManifestKind::Dir => 1,
            ManifestKind::Symlink => 2,
        });
        let mut flags = 0;
        if row.dirty {
            flags |= DIRTY;
        }
        if row.target.is_some() {
            flags |= HAS_TARGET;
        }
        body.push(flags);
        body.extend_from_slice(&row.mode.to_le_bytes());
        body.extend_from_slice(&row.size.to_le_bytes());
        body.extend_from_slice(&row.mtime_ns.to_le_bytes());
        if let Some(target) = &row.target {
            body.extend_from_slice(&(target.len() as u32).to_le_bytes());
            body.extend_from_slice(target.as_bytes());
        }
    }
    writer.write_all(MAGIC)?;
    writer.write_all(&(rows.len() as u64).to_le_bytes())?;
    writer.write_all(&checksum(&body).to_le_bytes())?;
    writer.write_all(&body)
}

/// Every row of a manifest, owned; see [`stream`].
pub struct ManifestReader {
    manifest: Manifest,
    next: usize,
}

impl Iterator for ManifestReader {
    type Item = Result<ManifestRow>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.manifest.len() {
            return None;
        }
        let row = self.manifest.row(self.next).to_row();
        self.next += 1;
        Some(Ok(row))
    }
}

/// Durably install a copy of the manifest file `source` as workspace `name`'s manifest, the
/// way [`build`] installs one it walked. Nothing is left behind when the copy fails.
pub fn install_copy(name: &str, source: &Path) -> Result {
    let path = manifest_path(name);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)
        .io_context(|| format!("create manifest directory {}", directory.display()))?;
    // The temporary name state recovery knows how to clean up (see `build_rows`).
    let temporary = directory.join(format!(".{name}.jsonl.tmp-{}", std::process::id()));
    // Manifests are only ever replaced by a rename, never written in place, so a hard link to
    // a durable `source` is as good as a copy and costs no data write. Another filesystem
    // gets a copy.
    let placed = match fs::hard_link(source, &temporary) {
        Ok(()) => Ok(()),
        Err(_) => fs::copy(source, &temporary)
            .io_context(|| format!("copy manifest {}", source.display()))
            .and_then(|_| crate::util::fs::sync_file(&temporary)),
    };
    let installed = placed
        .and_then(|()| {
            fs::rename(&temporary, &path)
                .io_context(|| format!("install manifest {}", path.display()))
        })
        .and_then(|()| crate::util::fs::sync_dir(directory));
    if installed.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    installed
}

/// Read workspace `name`'s manifest row by row, in path order.
pub fn stream(name: &str) -> Result<ManifestReader> {
    Ok(ManifestReader {
        manifest: Manifest::open(name)?,
        next: 0,
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
        return recover_unreadable(workspace, include_ignored);
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

/// A `Ready` manifest that cannot be read: build it again (see [`rebuildable`]), in the
/// background on macOS, else right here; otherwise tracking degrades.
fn recover_unreadable(workspace: &Workspace, include_ignored: bool) -> Result<ChangeCount> {
    if !rebuildable(workspace)? {
        let _ = mark(&workspace.name, ManifestStatus::Degraded);
        return Ok(ChangeCount::Degraded);
    }
    if can_spawn() {
        let _ = State::with_state(|state| {
            remove(&workspace.name)?;
            start_background(state, &workspace.name)
        });
        return Ok(ChangeCount::Pending);
    }
    if rebuild_now(workspace).unwrap_or(false) && Manifest::open(&workspace.name).is_ok() {
        let mut rebuilt = workspace.clone();
        rebuilt.manifest_status = ManifestStatus::Ready;
        rebuilt.manifest_builder = None;
        return change_count(&rebuilt, include_ignored);
    }
    Ok(ChangeCount::Pending)
}

/// Whether `workspace`'s manifest can be built again now, close to what the fork recorded:
/// a fresh walk of the view marks what was written since the fork dirty and recovers what
/// it lost from the base and the fork-time copy, which is what a slow background build does
/// anyway. After a pull the view says nothing about the baseline, so the fork-time copy must
/// exist. Without one, a lost manifest of a view written since the fork has lost entries
/// nothing can recover; a build that never finished is done as the background build would.
fn rebuildable(workspace: &Workspace) -> Result<bool> {
    if !workspace.backend_kind.stacking_is_snapshot() || !workspace.view_path.is_dir() {
        return Ok(false);
    }
    if crate::sync::ancestor::has_copy(workspace) {
        return Ok(true);
    }
    if was_pulled(workspace) {
        return Ok(false);
    }
    Ok(workspace.manifest_status == ManifestStatus::Building || !has_post_fork_writes(workspace)?)
}

/// Whether `workspace`'s baseline is a pulled base rather than its fork.
fn was_pulled(workspace: &Workspace) -> bool {
    workspace.last_pull.is_some() || workspace.manifest_generation > 1
}

/// Whether `workspace` needs its manifest built again before a command can use it: a
/// `building` status nobody is building any more, a `degraded` one, or a `ready` one whose
/// file is missing or damaged.
pub fn needs_rebuild(workspace: &Workspace) -> bool {
    if !workspace.backend_kind.stacking_is_snapshot() || !workspace.view_path.is_dir() {
        return false;
    }
    match workspace.manifest_status {
        ManifestStatus::Absent => false,
        ManifestStatus::Building => !build_in_progress(workspace),
        ManifestStatus::Degraded => true,
        ManifestStatus::Ready => Manifest::open(&workspace.name).is_err(),
    }
}

/// Whether a live process is still building `workspace`'s manifest.
pub fn build_in_progress(workspace: &Workspace) -> bool {
    workspace.manifest_status == ManifestStatus::Building
        && workspace
            .manifest_builder
            .as_ref()
            .is_some_and(Builder::is_alive)
}

/// Whether `workspace` waits on a background build whose process is gone, which the next
/// command restarts (see `State::reconcile_runtime`).
pub(crate) fn build_stalled(workspace: &Workspace) -> bool {
    workspace.manifest_status == ManifestStatus::Building
        && workspace.backend_kind.stacking_is_snapshot()
        && !build_in_progress(workspace)
        && workspace.view_path.is_dir()
}

/// Start a background build of workspace `name`'s manifest and record its process, with the
/// state lock held (`state` is saved by the caller).
pub(crate) fn start_background(state: &mut State, name: &str) -> Result {
    let workspace = state.require_workspace_mut(name)?;
    let builder = spawn(
        workspace.name.clone(),
        workspace.view_path.clone(),
        workspace.base.clone(),
        workspace.fork_time_ns,
    )?;
    workspace.manifest_status = ManifestStatus::Building;
    workspace.manifest_builder = Some(builder);
    Ok(())
}

/// A manifest built for a workspace and written next to its final place, not installed yet.
pub(crate) struct Staged {
    name: String,
    fork_time_ns: u128,
    temporary: PathBuf,
}

/// Build `workspace`'s manifest again from what is on disk (see [`rebuildable`]) and write it
/// beside its final place, for [`install`].
pub(crate) fn stage(workspace: &Workspace) -> Result<Staged> {
    let fork_copy = crate::sync::ancestor::has_copy(workspace)
        .then(|| crate::sync::ancestor::dir(&workspace.name));
    let rows = if was_pulled(workspace) {
        pulled_rows(
            &workspace.base,
            fork_copy.as_deref(),
            workspace.fork_time_ns,
        )
    } else {
        late_rows(
            &workspace.view_path,
            &workspace.base,
            fork_copy.as_deref(),
            workspace.fork_time_ns,
        )?
    };
    let temporary = temporary_path(&workspace.name);
    if let Err(error) = write_temporary(&temporary, rows, true) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(Staged {
        name: workspace.name.clone(),
        fork_time_ns: workspace.fork_time_ns,
        temporary,
    })
}

/// Install `staged` as its workspace's manifest and mark it ready, with the state lock held,
/// if the workspace still waits for it: the same fork (not a newer workspace of that name, or
/// one a pull moved on) and no usable manifest yet. Otherwise it is discarded. Returns whether
/// it was installed.
pub(crate) fn install(state: &mut State, staged: Staged) -> Result<bool> {
    let wanted = state.workspaces.get(&staged.name).is_some_and(|workspace| {
        workspace.fork_time_ns == staged.fork_time_ns
            && match workspace.manifest_status {
                ManifestStatus::Absent => false,
                ManifestStatus::Building | ManifestStatus::Degraded => true,
                ManifestStatus::Ready => Manifest::open(&staged.name).is_err(),
            }
    });
    if !wanted {
        let _ = fs::remove_file(&staged.temporary);
        return Ok(false);
    }
    let path = manifest_path(&staged.name);
    let installed = fs::rename(&staged.temporary, &path)
        .io_context(|| format!("install manifest {}", path.display()))
        .and_then(|()| crate::util::fs::sync_dir(path.parent().unwrap_or(Path::new("."))));
    if let Err(error) = installed {
        let _ = fs::remove_file(&staged.temporary);
        return Err(error);
    }
    let workspace = state.require_workspace_mut(&staged.name)?;
    workspace.manifest_status = ManifestStatus::Ready;
    workspace.manifest_builder = None;
    Ok(true)
}

/// Build `workspace`'s manifest in this process and install it, taking the state lock.
fn rebuild_now(workspace: &Workspace) -> Result<bool> {
    let staged = stage(workspace)?;
    let temporary = staged.temporary.clone();
    State::with_state(|state| install(state, staged)).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

/// Make workspace `name`'s manifest usable for a command that needs it (merge, pull, diff,
/// restore), with the state lock held: a build whose process is gone, a degraded status, or
/// a lost manifest is built again here, so a workspace never stays unusable when its
/// baseline can be recovered (STATE-02, STATE-03, STATE-11). A build still running is left
/// to finish; a workspace that cannot be rebuilt stays degraded, for the caller to refuse.
pub fn settle(state: &mut State, name: &str) -> Result {
    let Some(workspace) = state.workspaces.get(name).cloned() else {
        return Ok(());
    };
    if !needs_rebuild(&workspace) {
        return Ok(());
    }
    // An older forkspace that cannot read this manifest marks it degraded (UPG-01).
    if workspace.manifest_status == ManifestStatus::Degraded && Manifest::open(name).is_ok() {
        let workspace = state.require_workspace_mut(name)?;
        workspace.manifest_status = ManifestStatus::Ready;
        workspace.manifest_builder = None;
        return Ok(());
    }
    if !rebuildable(&workspace)? {
        return Ok(());
    }
    eprintln!("rebuilding the fork manifest of '{name}'");
    let staged = stage(&workspace).map_err(|error| {
        ForkspaceError::StateFailed(format!(
            "could not rebuild the fork manifest of '{name}': {error}"
        ))
    })?;
    install(state, staged)?;
    Ok(())
}

/// Build workspace `name`'s manifest in this process, for a fork whose background builder
/// could not start.
pub(crate) fn build_here(name: &str) -> Result {
    let Some(workspace) = State::peek()?.workspaces.get(name).cloned() else {
        return Ok(());
    };
    rebuild_now(&workspace).map(|_| ())
}

/// [`settle`] for a caller that does not hold the state lock; the lock is taken only when
/// something needs doing. With `wait`, a build still running is waited for first, as long as
/// its process lives.
pub fn settle_now(name: &str, wait: bool) -> Result {
    let mut told = false;
    let needed = loop {
        let state = State::peek()?;
        let Some(workspace) = state.workspaces.get(name) else {
            return Ok(());
        };
        if !(wait && build_in_progress(workspace)) {
            break needs_rebuild(workspace);
        }
        if !told {
            eprintln!("waiting for the fork manifest of '{name}' to be built");
            told = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    if needed {
        State::with_state(|state| settle(state, name))?;
    }
    Ok(())
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
        if written_after_fork(&Entry::of(&metadata), workspace.fork_time_ns) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, kind: ManifestKind, target: Option<&str>, dirty: bool) -> ManifestRow {
        ManifestRow {
            path: path.to_string(),
            kind,
            size: path.len() as u64,
            mtime_ns: -1_700_000_000_123_456_789,
            mode: 0o644,
            target: target.map(str::to_string),
            dirty,
        }
    }

    fn sample() -> Vec<ManifestRow> {
        vec![
            row("src/b.ts", ManifestKind::File, None, false),
            row("src", ManifestKind::Dir, None, false),
            row("link", ManifestKind::Symlink, Some("src/b.ts"), false),
            row("src/a%20b.ts", ManifestKind::File, None, true),
            row("src-other", ManifestKind::File, None, false),
        ]
    }

    #[test]
    fn binary_manifest_round_trips_and_finds_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m");
        let mut file = Vec::new();
        write_binary(&mut file, &mut sample()).unwrap();
        fs::write(&path, &file).unwrap();

        let manifest = Manifest::open_file(&path).unwrap();
        let mut expected = sample();
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        let read: Vec<ManifestRow> = manifest.iter().map(|row| row.to_row()).collect();
        assert_eq!(read, expected);
        assert_eq!(manifest.get("link").unwrap().target, Some("src/b.ts"));
        assert!(manifest.get("src/a%20b.ts").unwrap().dirty);
        assert!(manifest.get("missing").is_none());
        let under: Vec<&str> = manifest.under("src").map(|row| row.path).collect();
        assert_eq!(under, ["src/a%20b.ts", "src/b.ts"]);
    }

    #[test]
    fn json_lines_manifests_are_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m");
        let lines: String = sample()
            .iter()
            .map(|row| serde_json::to_string(row).unwrap() + "\n")
            .collect();
        fs::write(&path, lines).unwrap();
        let manifest = Manifest::open_file(&path).unwrap();
        assert_eq!(manifest.len(), 5);
        assert_eq!(manifest.get("src/b.ts").unwrap().to_row(), sample()[0]);
    }

    #[test]
    fn a_damaged_binary_manifest_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m");
        let mut file = Vec::new();
        write_binary(&mut file, &mut sample()).unwrap();
        for cut in [9, HEADER + 3, file.len() - 1] {
            fs::write(&path, &file[..cut]).unwrap();
            assert!(matches!(
                Manifest::open_file(&path),
                Err(ForkspaceError::StateCorrupted(_))
            ));
        }
        let mut flipped = file.clone();
        let last = flipped.len() - 5;
        flipped[last] ^= 0x40;
        fs::write(&path, &flipped).unwrap();
        assert!(matches!(
            Manifest::open_file(&path),
            Err(ForkspaceError::StateCorrupted(_))
        ));
    }
}
