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
    build_rows(name, view, None, fork_time_ns)
}

/// [`build`] from entries a copy recorded while it created `view` (see
/// `materialize_recorded`), instead of walking `view`. Nothing was written after the fork
/// yet, so no row is dirty.
pub(crate) fn build_from_entries(
    name: &str,
    view: &Path,
    entries: &[(PathBuf, Entry)],
) -> Result<usize> {
    let path = manifest_path(name);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = directory.join(format!(".{name}.jsonl.tmp-{}", std::process::id()));
    let rows = entries
        .iter()
        .map(|(relative, entry)| row_of(relative, &view.join(relative), entry, u128::MAX))
        .collect::<Result<Vec<_>>>()?;
    write_file(&path, &temporary, rows, true)
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
    // State recovery deletes `.<name>.jsonl.tmp-<pid>` leftovers once `<pid>` has exited, so
    // an interrupted build is cleaned up later. Keep this exact name.
    let temporary = directory.join(format!(".{name}.jsonl.tmp-{}", std::process::id()));
    let rows = collect_rows(view, base, fork_time_ns)?;
    write_file(&path, &temporary, rows, true)
}

/// Install `rows` as the binary manifest at `path`, written first to `temporary` in the same
/// directory. With `durable`, the file and the rename are synced before this returns; without,
/// the caller flushes them later. Returns the row count.
pub(crate) fn write_file(
    path: &Path,
    temporary: &Path,
    mut rows: Vec<ManifestRow>,
    durable: bool,
) -> Result<usize> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
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
    fs::rename(temporary, path).io_context(|| format!("install manifest {}", path.display()))?;
    if durable {
        crate::util::fs::sync_dir(directory)?;
    }
    Ok(rows.len())
}

/// The rows of every entry in `view`, with base entries the view lost after the fork when
/// `base` is given (see [`build_after_fork`]).
fn collect_rows(view: &Path, base: Option<&Path>, fork_time_ns: u128) -> Result<Vec<ManifestRow>> {
    let Some(base) = base else {
        // At fork time the view is not in use yet: read it on several threads.
        let rows = crate::util::walk::parallel(view, |relative, path, metadata| {
            row_of(relative, path, metadata, fork_time_ns).map(Some)
        })?;
        return Ok(rows.into_iter().map(|(_, row)| row).collect());
    };
    let mut rows = Vec::new();
    for entry in WalkDir::new(view).follow_links(false) {
        let entry = entry.walk_context(view)?;
        if entry.depth() > 0 {
            rows.push(row(view, &entry, fork_time_ns)?);
        }
        let changed_dir = entry.file_type().is_dir()
            && entry
                .metadata()
                .is_ok_and(|metadata| written_after_fork(&Entry::of(&metadata), fork_time_ns));
        if changed_dir {
            let relative = entry.path().strip_prefix(view).unwrap_or(Path::new(""));
            for missing in entries_missing_from_view(view, base, relative, fork_time_ns)? {
                rows.push(row(base, &missing, fork_time_ns)?);
            }
        }
    }
    Ok(rows)
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
                .is_ok_and(|metadata| !written_after_fork(&Entry::of(&metadata), fork_time_ns))
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
        let index = self.lower_bound(encoded);
        (index < self.len())
            .then(|| self.row(index))
            .filter(|row| row.path == encoded)
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
            if self.row(middle).path.as_bytes() < encoded.as_bytes() {
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
