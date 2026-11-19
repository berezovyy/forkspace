//! `merge --patch`: a workspace's changes as a Git patch that `git apply` reproduces byte for
//! byte, or the list of changes no such patch can carry.
//!
//! Content is diffed as bytes, so CRLF line ends, a missing final newline and text that is not
//! UTF-8 come through unchanged. Executable bits, symlinks, empty files and deleted directories
//! use Git's extended headers. Binary content, special files, empty new directories, and changes
//! whose fork-time version is unknown cannot be written as a text patch: [`git_patch`] returns
//! them instead of a patch that silently leaves them out.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use similar::{Algorithm, DiffTag};
use walkdir::WalkDir;

use super::manifest::{decode_path, encode_path, Manifest, ManifestKind, ManifestStatus};
use super::{ChangeKind, FileChange};
use crate::backend::BackendState;
use crate::sync::plan::matches_ancestor;
use crate::workspace::Workspace;

/// Unchanged lines kept around each hunk, as `git diff` does.
const CONTEXT: usize = 3;

/// A change the patch cannot carry, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Omission {
    pub path: PathBuf,
    pub reason: String,
}

/// One version of a path, as far as a patch is concerned.
enum Entry {
    Missing,
    /// A file; `binary` (with empty `bytes`) when its first bytes hold a NUL, so it is never
    /// read whole.
    File {
        executable: bool,
        bytes: Vec<u8>,
        binary: Option<PathBuf>,
    },
    Symlink(Vec<u8>),
    Dir(PathBuf),
}

/// The patch for `changes`, whose new versions live under `source` (the directory a merge copies
/// from). Every change is either carried by the patch or returned as an [`Omission`]; a patch is
/// only returned when nothing is omitted.
pub fn git_patch(
    workspace: &Workspace,
    source: &Path,
    changes: &[FileChange],
) -> std::result::Result<Vec<u8>, Vec<Omission>> {
    let snapshot = matches!(
        workspace.backend_state,
        BackendState::MacClone { .. } | BackendState::CowDir { .. }
    );
    let manifest = (snapshot && workspace.manifest_status == ManifestStatus::Ready)
        .then(|| Manifest::open(&workspace.name).ok())
        .flatten();
    let export = Export {
        workspace,
        source,
        listed: changes.iter().map(|change| change.path.as_path()).collect(),
        snapshot,
        manifest,
    };
    let mut patch = Vec::new();
    let mut omitted = Vec::new();
    for change in changes {
        let mut section = Vec::new();
        match export.render_change(&mut section, change) {
            Ok(()) => patch.extend_from_slice(&section),
            Err(omission) => omitted.push(omission),
        }
    }
    if omitted.is_empty() {
        Ok(patch)
    } else {
        Err(omitted)
    }
}

fn omission(path: &Path, reason: impl Into<String>) -> Omission {
    Omission {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

/// What every change of one export shares.
struct Export<'a> {
    workspace: &'a Workspace,
    source: &'a Path,
    /// Paths that are changes of their own: a directory expansion leaves them to their change,
    /// so nothing is deleted twice.
    listed: HashSet<&'a Path>,
    /// A directory-snapshot workspace (mac-clone or copy), whose fork-time state is the manifest
    /// plus the fork-time copy; an overlay's is its frozen lower layer.
    snapshot: bool,
    /// The snapshot's fork manifest, when it is ready and readable.
    manifest: Option<Manifest>,
}

impl Export<'_> {
    fn render_change(
        &self,
        out: &mut Vec<u8>,
        change: &FileChange,
    ) -> std::result::Result<(), Omission> {
        let old = if change.kind == ChangeKind::Added {
            Entry::Missing
        } else {
            let Some(path) = super::old_version(self.workspace, change) else {
                return Err(omission(
                    &change.path,
                    "its fork-time version is unknown: the base no longer holds it and no fork-time copy was kept",
                ));
            };
            read(&path).map_err(|reason| omission(&change.path, reason))?
        };
        let new = if change.kind == ChangeKind::Deleted {
            Entry::Missing
        } else {
            read(&self.source.join(&change.path))
                .map_err(|reason| omission(&change.path, reason))?
        };
        self.emit(out, &change.path, old, new)
    }

    /// Write the patch that turns `old` into `new` at `path`.
    fn emit(
        &self,
        out: &mut Vec<u8>,
        path: &Path,
        old: Entry,
        new: Entry,
    ) -> std::result::Result<(), Omission> {
        match (old, new) {
            (Entry::Missing, Entry::Missing) | (Entry::Dir(_), Entry::Dir(_)) => Ok(()),
            // A new directory's entries are changes of their own; an empty one has none, and
            // Git has no way to create it.
            (Entry::Missing, Entry::Dir(directory)) => {
                let empty =
                    fs::read_dir(&directory).map_or(true, |mut entries| entries.next().is_none());
                if empty {
                    return Err(omission(
                        path,
                        "an empty new directory cannot be created by a patch",
                    ));
                }
                Ok(())
            }
            // A deleted directory is reported once; the patch deletes what it held at fork time.
            (Entry::Dir(directory), new) => {
                self.delete_directory(out, path, &directory)?;
                self.emit(out, path, Entry::Missing, new)
            }
            (old, Entry::Dir(directory)) => {
                self.emit(out, path, old, Entry::Missing)?;
                self.emit(out, path, Entry::Missing, Entry::Dir(directory))
            }
            (old, new) => {
                let old = Version::of(old);
                let new = Version::of(new);
                // Git writes a change between a file and a symlink as a deletion and a creation.
                if let (Some(before), Some(after)) = (&old, &new) {
                    if before.symlink != after.symlink {
                        section(out, path, old.as_ref(), None)?;
                        return section(out, path, None, new.as_ref());
                    }
                }
                section(out, path, old.as_ref(), new.as_ref())
            }
        }
    }

    /// Delete every file and symlink directory `path` held at fork time, with its fork-time
    /// bytes, so `git apply` refuses rather than deletes anything the base changed since.
    fn delete_directory(
        &self,
        out: &mut Vec<u8>,
        path: &Path,
        old_directory: &Path,
    ) -> std::result::Result<(), Omission> {
        for (relative, version) in self.fork_time_entries(path, old_directory)? {
            // Left to its own change, or to the change of a directory under `path` that holds it.
            let listed = relative
                .ancestors()
                .take_while(|ancestor| *ancestor != path)
                .any(|ancestor| self.listed.contains(ancestor));
            if listed {
                continue;
            }
            let old = read(&version).map_err(|reason| omission(&relative, reason))?;
            if matches!(old, Entry::Missing | Entry::Dir(_)) {
                return Err(omission(
                    &relative,
                    "its fork-time version could not be read",
                ));
            }
            self.emit(out, &relative, old, Entry::Missing)?;
        }
        Ok(())
    }

    /// The files and symlinks under `path` at fork time, each with where its fork-time version
    /// can be read. An empty directory among them cannot be deleted by a patch.
    fn fork_time_entries(
        &self,
        path: &Path,
        old_directory: &Path,
    ) -> std::result::Result<Vec<(PathBuf, PathBuf)>, Omission> {
        if !self.snapshot {
            // An overlay's old side is its frozen lower layer: complete, and fork-time.
            return walk_tree(path, old_directory);
        }
        let encoded = encode_path(path);
        if let Some(manifest) = &self.manifest {
            if manifest
                .get(&encoded)
                .is_some_and(|row| row.kind == ManifestKind::Dir && !row.dirty)
            {
                return self.entries_from_manifest(manifest, &encoded);
            }
        }
        // The manifest walked this directory after it changed: list the fork-time copy, which
        // leaves out ignored directories. Anything in the base it does not cover may or may not
        // have been there at fork, so the deletion is not known in full.
        let copy = crate::sync::ancestor::entry(self.workspace, path)
            .filter(|copy| copy.is_dir())
            .ok_or_else(|| omission(path, "the directory's fork-time contents are unknown"))?;
        let entries = walk_tree(path, &copy)?;
        let base = self.workspace.base.join(path);
        if base.is_dir() {
            for entry in WalkDir::new(&base).min_depth(1).follow_links(false) {
                let entry = entry.map_err(|error| {
                    omission(path, format!("cannot list the base directory: {error}"))
                })?;
                let inner = entry.path().strip_prefix(&base).unwrap_or(entry.path());
                if fs::symlink_metadata(copy.join(inner)).is_err() {
                    return Err(omission(
                        &path.join(inner),
                        "it is in the base but not in the fork-time copy, so whether the deleted directory held it is unknown",
                    ));
                }
            }
        }
        Ok(entries)
    }

    fn entries_from_manifest(
        &self,
        manifest: &Manifest,
        encoded: &str,
    ) -> std::result::Result<Vec<(PathBuf, PathBuf)>, Omission> {
        let mut entries = Vec::new();
        for index in manifest.lower_bound(&format!("{encoded}/"))..manifest.end_of_subtree(encoded)
        {
            let row = manifest.row(index);
            let relative = decode_path(row.path).map_err(|_| {
                omission(Path::new(row.path), "its recorded name cannot be decoded")
            })?;
            if row.kind == ManifestKind::Dir {
                let first_child = manifest.lower_bound(&format!("{}/", row.path));
                let has_child = first_child < manifest.len()
                    && manifest
                        .row(first_child)
                        .path
                        .starts_with(&format!("{}/", row.path));
                if !has_child {
                    return Err(omission(
                        &relative,
                        "an empty directory cannot be deleted by a patch",
                    ));
                }
                continue;
            }
            let base = self.workspace.base.join(&relative);
            let version = if !row.dirty && matches_ancestor(&base, &row.to_row()) {
                base
            } else {
                crate::sync::ancestor::entry(self.workspace, &relative).ok_or_else(|| {
                    omission(&relative, "its fork-time version is unknown: the base changed it and no fork-time copy was kept")
                })?
            };
            entries.push((relative, version));
        }
        Ok(entries)
    }
}

/// The files and symlinks under `directory` (which is `path` in the workspace), sorted.
fn walk_tree(
    path: &Path,
    directory: &Path,
) -> std::result::Result<Vec<(PathBuf, PathBuf)>, Omission> {
    let mut entries = Vec::new();
    for entry in WalkDir::new(directory)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|error| {
            omission(path, format!("cannot list the deleted directory: {error}"))
        })?;
        let relative = path.join(entry.path().strip_prefix(directory).unwrap_or(entry.path()));
        let kind = entry.file_type();
        if kind.is_dir() {
            if fs::read_dir(entry.path()).map_or(true, |mut inner| inner.next().is_none()) {
                return Err(omission(
                    &relative,
                    "an empty directory cannot be deleted by a patch",
                ));
            }
            continue;
        }
        if !(kind.is_file() || kind.is_symlink()) {
            return Err(omission(
                &relative,
                "not a regular file, directory or symlink",
            ));
        }
        entries.push((relative, entry.path().to_path_buf()));
    }
    if entries.is_empty() {
        return Err(omission(
            path,
            "an empty directory cannot be deleted by a patch",
        ));
    }
    Ok(entries)
}

fn read(path: &Path) -> std::result::Result<Entry, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Entry::Missing),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return fs::read_link(path)
            .map(|target| Entry::Symlink(target.as_os_str().as_bytes().to_vec()))
            .map_err(|error| format!("cannot read symlink {}: {error}", path.display()));
    }
    if kind.is_dir() {
        return Ok(Entry::Dir(path.to_path_buf()));
    }
    if !kind.is_file() {
        return Err("not a regular file, directory or symlink".to_string());
    }
    let executable = metadata.permissions().mode() & 0o111 != 0;
    // Checked before reading, so a large binary is never read whole (DIFF-11).
    if super::content::is_binary(path) {
        return Ok(Entry::File {
            executable,
            bytes: Vec::new(),
            binary: Some(path.to_path_buf()),
        });
    }
    if metadata.len() > super::content::MAX_TEXT_DIFF_BYTES {
        return Err(format!(
            "it is {} MiB, over the {} MiB limit for a text patch; merge it with `forkspace merge`",
            metadata.len() >> 20,
            super::content::MAX_TEXT_DIFF_BYTES >> 20
        ));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    Ok(Entry::File {
        executable,
        bytes,
        binary: None,
    })
}

/// A file or symlink as Git sees it: a mode and the bytes it stores.
struct Version {
    mode: &'static str,
    symlink: bool,
    bytes: Vec<u8>,
    /// A binary file's path; its bytes are not read.
    binary: Option<PathBuf>,
}

impl Version {
    fn of(entry: Entry) -> Option<Self> {
        match entry {
            Entry::File {
                executable,
                bytes,
                binary,
            } => Some(Self {
                mode: if executable { "100755" } else { "100644" },
                symlink: false,
                bytes,
                binary,
            }),
            Entry::Symlink(target) => Some(Self {
                mode: "120000",
                symlink: true,
                bytes: target,
                binary: None,
            }),
            Entry::Missing | Entry::Dir(_) => None,
        }
    }
}

/// The path of a binary version, whose bytes were not read.
fn binary_path(version: Option<&Version>) -> Option<&Path> {
    version.and_then(|version| version.binary.as_deref())
}

/// One `diff --git` section. Writes nothing when Git would see no change (same mode and
/// bytes, e.g. a file that was only touched).
fn section(
    out: &mut Vec<u8>,
    path: &Path,
    old: Option<&Version>,
    new: Option<&Version>,
) -> std::result::Result<(), Omission> {
    let empty: &[u8] = &[];
    let old_bytes = old.map_or(empty, |version| version.bytes.as_slice());
    let new_bytes = new.map_or(empty, |version| version.bytes.as_slice());
    let content_changed = match (binary_path(old), binary_path(new)) {
        (None, None) => old_bytes != new_bytes,
        (Some(before), Some(after)) => !crate::util::fs::same_contents(before, after),
        _ => true,
    };
    let mode_changed = matches!((old, new), (Some(a), Some(b)) if a.mode != b.mode);
    if old.is_some() && new.is_some() && !content_changed && !mode_changed {
        return Ok(());
    }
    let any_binary = binary_path(old).is_some() || binary_path(new).is_some();
    if content_changed && (any_binary || old_bytes.contains(&0) || new_bytes.contains(&0)) {
        return Err(Omission {
            path: path.to_path_buf(),
            reason: "binary content cannot be carried by a text patch".to_string(),
        });
    }

    out.extend_from_slice(b"diff --git ");
    out.extend_from_slice(&quote("a/", path));
    out.push(b' ');
    out.extend_from_slice(&quote("b/", path));
    out.push(b'\n');
    match (old, new) {
        (None, Some(version)) => {
            out.extend_from_slice(format!("new file mode {}\n", version.mode).as_bytes());
        }
        (Some(version), None) => {
            out.extend_from_slice(format!("deleted file mode {}\n", version.mode).as_bytes());
        }
        (Some(before), Some(after)) if mode_changed => {
            out.extend_from_slice(
                format!("old mode {}\nnew mode {}\n", before.mode, after.mode).as_bytes(),
            );
        }
        _ => {}
    }
    if !content_changed {
        return Ok(());
    }
    out.extend_from_slice(b"--- ");
    match old {
        Some(_) => out.extend_from_slice(&quote("a/", path)),
        None => out.extend_from_slice(b"/dev/null"),
    }
    out.extend_from_slice(b"\n+++ ");
    match new {
        Some(_) => out.extend_from_slice(&quote("b/", path)),
        None => out.extend_from_slice(b"/dev/null"),
    }
    out.push(b'\n');
    hunks(out, old_bytes, new_bytes);
    Ok(())
}

/// One line of a diff: kept, or only on the old or the new side.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Line {
    Kept,
    Old,
    New,
}

/// Unified hunks between two byte strings, split after each `\n`.
///
/// The diff's operations are read for their kind and lengths only, and every position is
/// counted from the start of the file: the index a deletion or insertion gives for the *other*
/// side can lag, which put hunk headers out of step with their bodies (MERGE-26).
fn hunks(out: &mut Vec<u8>, old: &[u8], new: &[u8]) {
    let old_lines: Vec<&[u8]> = old.split_inclusive(|&byte| byte == b'\n').collect();
    let new_lines: Vec<&[u8]> = new.split_inclusive(|&byte| byte == b'\n').collect();
    let ops = similar::capture_diff_slices(Algorithm::Myers, &old_lines, &new_lines);
    // Each line in order, with how many old and new lines come before it.
    let mut lines: Vec<(Line, usize, usize)> = Vec::new();
    let (mut old_at, mut new_at) = (0, 0);
    let mut push = |kind: Line, count: usize, lines: &mut Vec<(Line, usize, usize)>| {
        for _ in 0..count {
            lines.push((kind, old_at, new_at));
            if kind != Line::New {
                old_at += 1;
            }
            if kind != Line::Old {
                new_at += 1;
            }
        }
    };
    for op in &ops {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => push(Line::Kept, old_range.len(), &mut lines),
            DiffTag::Delete => push(Line::Old, old_range.len(), &mut lines),
            DiffTag::Insert => push(Line::New, new_range.len(), &mut lines),
            DiffTag::Replace => {
                push(Line::Old, old_range.len(), &mut lines);
                push(Line::New, new_range.len(), &mut lines);
            }
        }
    }
    let changed: Vec<usize> = (0..lines.len())
        .filter(|&index| lines[index].0 != Line::Kept)
        .collect();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &index in &changed {
        match groups.last_mut() {
            // Close enough that their context would touch: one hunk.
            Some((_, last)) if index - *last <= 2 * CONTEXT + 1 => *last = index,
            _ => groups.push((index, index)),
        }
    }
    for (first, last) in groups {
        let start = first.saturating_sub(CONTEXT);
        let end = (last + CONTEXT + 1).min(lines.len());
        let span = &lines[start..end];
        let old_len = span.iter().filter(|line| line.0 != Line::New).count();
        let new_len = span.iter().filter(|line| line.0 != Line::Old).count();
        let (_, old_start, new_start) = lines[start];
        out.extend_from_slice(
            format!(
                "@@ -{} +{} @@\n",
                hunk_range(&(old_start..old_start + old_len)),
                hunk_range(&(new_start..new_start + new_len))
            )
            .as_bytes(),
        );
        for &(kind, old_index, new_index) in span {
            match kind {
                Line::Kept => line(out, b' ', old_lines[old_index]),
                Line::Old => line(out, b'-', old_lines[old_index]),
                Line::New => line(out, b'+', new_lines[new_index]),
            }
        }
    }
}

/// `start,len` of a hunk side, 1-based; an empty side names the line before it.
fn hunk_range(range: &std::ops::Range<usize>) -> String {
    match range.len() {
        0 => format!("{},0", range.start),
        1 => format!("{}", range.start + 1),
        len => format!("{},{len}", range.start + 1),
    }
}

fn line(out: &mut Vec<u8>, prefix: u8, text: &[u8]) {
    out.push(prefix);
    out.extend_from_slice(text);
    if !text.ends_with(b"\n") {
        out.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
}

/// `prefix` and `path` as Git writes a name: bare, or C-quoted when it holds a quote, a
/// backslash, a control byte or a byte outside ASCII.
fn quote(prefix: &str, path: &Path) -> Vec<u8> {
    let bytes = path.as_os_str().as_bytes();
    let plain = |byte: u8| (0x20..0x7f).contains(&byte) && byte != b'"' && byte != b'\\';
    let mut out = Vec::with_capacity(prefix.len() + bytes.len() + 2);
    if bytes.iter().all(|&byte| plain(byte)) {
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(bytes);
        return out;
    }
    out.push(b'"');
    out.extend_from_slice(prefix.as_bytes());
    for &byte in bytes {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            0x0b => out.extend_from_slice(b"\\v"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\r' => out.extend_from_slice(b"\\r"),
            byte if plain(byte) => out.push(byte),
            byte => out.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
        }
    }
    out.push(b'"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk_text(old: &str, new: &str) -> String {
        let mut out = Vec::new();
        hunks(&mut out, old.as_bytes(), new.as_bytes());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn hunks_keep_line_ends_and_mark_a_missing_final_newline() {
        assert_eq!(
            hunk_text("a\r\nb\r\n", "a\r\nB\r\n"),
            "@@ -1,2 +1,2 @@\n a\r\n-b\r\n+B\r\n"
        );
        assert_eq!(
            hunk_text("old\n", "new"),
            "@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n"
        );
        assert_eq!(hunk_text("", "x\n"), "@@ -0,0 +1 @@\n+x\n");
        assert_eq!(hunk_text("x\ny\n", ""), "@@ -1,2 +0,0 @@\n-x\n-y\n");
    }

    /// MERGE-26: a hunk that starts with a deletion, as `git diff` writes it.
    #[test]
    fn a_hunk_starting_with_a_deletion_counts_from_the_first_line() {
        let new = "gamma\ngamma\ngamma\neta1\neta\nalpha0\nkappa\nkappa\nkappa\nkappa\n";
        let text = hunk_text("zeta\ngamma\nzeta\n", new);
        assert!(
            text.starts_with("@@ -1,3 +1,10 @@\n-zeta\n gamma\n-zeta\n"),
            "{text}"
        );
    }

    #[test]
    fn hunks_keep_three_lines_of_context() {
        let old: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let new = old.replace("10\n", "ten\n");
        assert_eq!(
            hunk_text(&old, &new),
            "@@ -7,7 +7,7 @@\n 7\n 8\n 9\n-10\n+ten\n 11\n 12\n 13\n"
        );
    }

    /// A small deterministic generator, so the property test needs no extra crate.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> usize {
            (self.next() % bound) as usize
        }
    }

    /// Random lines from a small alphabet, so equal lines recur and diffs interleave; the last
    /// line sometimes has no newline.
    fn random_text(rng: &mut Rng, lines: usize) -> Vec<u8> {
        let mut text = Vec::new();
        for _ in 0..lines {
            text.push(b"abcde"[rng.below(5)]);
            text.push(b'\n');
        }
        if !text.is_empty() && rng.below(4) == 0 {
            text.pop();
        }
        text
    }

    /// \`old\` with random lines deleted, replaced and inserted.
    fn mutate(rng: &mut Rng, old: &[u8]) -> Vec<u8> {
        let mut lines: Vec<Vec<u8>> = old
            .split_inclusive(|&byte| byte == b'\n')
            .map(<[u8]>::to_vec)
            .collect();
        for _ in 0..=rng.below(6) {
            let at = rng.below(lines.len() as u64 + 1);
            match rng.below(3) {
                0 if at < lines.len() => {
                    lines.remove(at);
                }
                1 if at < lines.len() => lines[at] = vec![b"vwxyz"[rng.below(5)], b'\n'],
                _ => {
                    for _ in 0..=rng.below(3) {
                        lines.insert(at, vec![b"abvw"[rng.below(4)], b'\n']);
                    }
                }
            }
        }
        let mut text = lines.concat();
        if !text.is_empty() && rng.below(4) == 0 && text.ends_with(b"\n") {
            text.pop();
        }
        text
    }

    /// \`(start, len)\` of one side of a hunk header, as written.
    fn side(text: &str) -> (usize, usize) {
        match text.split_once(',') {
            Some((start, len)) => (start.parse().unwrap(), len.parse().unwrap()),
            None => (text.parse().unwrap(), 1),
        }
    }

    /// Apply \`patch\` (hunks only) to \`old\` exactly where its headers say, as \`git apply\`
    /// does without fuzz: every context and deleted line must be at its stated position, and
    /// every hunk's counts must match its body. Panics on any disagreement.
    fn apply_strictly(old: &[u8], patch: &[u8]) -> Vec<u8> {
        let old_lines: Vec<&[u8]> = old.split_inclusive(|&byte| byte == b'\n').collect();
        let mut body: Vec<(u8, Vec<u8>)> = Vec::new();
        type Hunk = ((usize, usize), (usize, usize), Vec<(u8, Vec<u8>)>);
        let mut hunks: Vec<Hunk> = Vec::new();
        for raw in patch.split_inclusive(|&byte| byte == b'\n') {
            if raw.starts_with(b"@@ ") {
                if let Some(last) = hunks.last_mut() {
                    last.2 = std::mem::take(&mut body);
                }
                let header = std::str::from_utf8(raw).unwrap();
                let mut parts = header.split(' ');
                let old_side = side(parts.nth(1).unwrap().trim_start_matches('-'));
                let new_side = side(parts.next().unwrap().trim_start_matches('+'));
                hunks.push((old_side, new_side, Vec::new()));
            } else if raw.starts_with(b"\\") {
                let previous = body.last_mut().unwrap();
                assert_eq!(previous.1.pop(), Some(b'\n'));
            } else {
                body.push((raw[0], raw[1..].to_vec()));
            }
        }
        if let Some(last) = hunks.last_mut() {
            last.2 = body;
        }
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut at = 0;
        for ((old_start, old_len), (new_start, new_len), lines) in hunks {
            let old_first = if old_len == 0 {
                old_start
            } else {
                old_start - 1
            };
            let new_first = if new_len == 0 {
                new_start
            } else {
                new_start - 1
            };
            assert!(old_first >= at, "hunks overlap or go backwards");
            out.extend(old_lines[at..old_first].iter().map(|line| line.to_vec()));
            assert_eq!(
                out.len(),
                new_first,
                "new-side start disagrees with the body"
            );
            at = old_first;
            let (mut olds, mut news) = (0, 0);
            for (prefix, text) in lines {
                if prefix != b'+' {
                    assert_eq!(old_lines[at], text.as_slice(), "old line {at} disagrees");
                    at += 1;
                    olds += 1;
                }
                if prefix != b'-' {
                    out.push(text);
                    news += 1;
                }
            }
            assert_eq!((olds, news), (old_len, new_len), "header counts disagree");
        }
        out.extend(old_lines[at..].iter().map(|line| line.to_vec()));
        out.concat()
    }

    /// MERGE-26: over random line diffs, every hunk header agrees with its body, and applying
    /// the hunks where the headers say reproduces the new text.
    #[test]
    fn hunk_headers_agree_with_their_bodies() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..20_000 {
            let lines = rng.below(30);
            let old = random_text(&mut rng, lines);
            let new = mutate(&mut rng, &old);
            let mut patch = Vec::new();
            hunks(&mut patch, &old, &new);
            let applied = std::panic::catch_unwind(|| apply_strictly(&old, &patch));
            assert!(
                applied.as_ref().is_ok_and(|applied| *applied == new),
                "case {case}\nold: {:?}\nnew: {:?}\npatch:\n{}",
                String::from_utf8_lossy(&old),
                String::from_utf8_lossy(&new),
                String::from_utf8_lossy(&patch)
            );
        }
    }

    #[test]
    fn names_are_quoted_like_git() {
        assert_eq!(quote("a/", Path::new("src/a b.rs")), b"a/src/a b.rs");
        assert_eq!(quote("b/", Path::new("caf\u{e9}")), b"\"b/caf\\303\\251\"");
        assert_eq!(
            quote("a/", Path::new("x\"y\\z\tw")),
            b"\"a/x\\\"y\\\\z\\tw\""
        );
    }
}
