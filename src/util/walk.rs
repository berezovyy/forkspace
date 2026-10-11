//! Parallel walks for the scans that must visit a whole tree, such as manifest builds, the
//! frozen-copy freshness check, change scans, and disk use. Directories are read on several
//! threads at once, which is what makes metadata-heavy walks of large `node_modules` trees
//! fast.
//!
//! Linux reads directories with `ignore`'s parallel walker and one `lstat` per entry. macOS
//! reads each directory with `getattrlistbulk(2)`, which returns the names and attributes of
//! many entries in one call (see [`super::bulk`]).

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{ForkspaceError, Result};

/// What a [`parallel_with`] visitor wants done with an entry.
pub enum Step<T> {
    /// Keep the item (if any) and, for a directory, descend into it.
    Descend(Option<T>),
    /// Keep the item (if any) but do not descend into this directory.
    Prune(Option<T>),
}

/// The kind of a walked entry (symlinks are not followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// The `lstat` facts the scans use about one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub len: u64,
    /// Permission bits (`mode & 0o7777`).
    pub mode: u32,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    /// Allocated 512-byte blocks.
    pub blocks: u64,
}

impl Entry {
    pub fn of(metadata: &fs::Metadata) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            Kind::Symlink
        } else if file_type.is_dir() {
            Kind::Dir
        } else if file_type.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        Self {
            kind,
            len: metadata.len(),
            mode: metadata.mode() & 0o7777,
            mtime_ns: super::fs::mtime_ns(metadata),
            ctime_ns: super::fs::ctime_ns(metadata),
            dev: metadata.dev(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
            blocks: metadata.blocks(),
        }
    }

    /// `lstat` of `path`.
    pub fn lstat(path: &Path) -> Result<Self> {
        fs::symlink_metadata(path)
            .map(|metadata| Self::of(&metadata))
            .map_err(|source| ForkspaceError::Io {
                context: format!("inspect {}", path.display()),
                source,
            })
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn is_file(&self) -> bool {
        self.kind == Kind::File
    }

    pub fn is_symlink(&self) -> bool {
        self.kind == Kind::Symlink
    }
}

/// Visit every entry under `root` (not `root` itself) without following symlinks and without
/// honouring ignore files. `visit` receives the path relative to `root`, the full path, and
/// the entry's facts, and may keep an item; the first error stops the walk and is returned.
/// Kept items come back sorted by relative path, so output does not depend on thread timing.
pub fn parallel<T, F>(root: &Path, visit: F) -> Result<Vec<(PathBuf, T)>>
where
    T: Send,
    F: Fn(&Path, &Path, &Entry) -> Result<Option<T>> + Sync,
{
    parallel_with(root, |relative, path, entry| {
        visit(relative, path, entry).map(Step::Descend)
    })
}

/// [`parallel`] whose visitor can also prune directories.
pub fn parallel_with<T, F>(root: &Path, visit: F) -> Result<Vec<(PathBuf, T)>>
where
    T: Send,
    F: Fn(&Path, &Path, &Entry) -> Result<Step<T>> + Sync,
{
    let kept = Mutex::new(Vec::new());
    let keep = |relative: &Path, path: &Path, entry: &Entry| -> Result<bool> {
        let (item, descend) = match visit(relative, path, entry)? {
            Step::Descend(item) => (item, true),
            Step::Prune(item) => (item, false),
        };
        if let Some(item) = item {
            kept.lock()
                .expect("walk results lock")
                .push((relative.to_path_buf(), item));
        }
        Ok(descend)
    };
    walk(root, &keep)?;
    let mut kept = kept.into_inner().expect("walk results lock");
    kept.sort_by(|a, b| {
        a.0.as_os_str()
            .as_encoded_bytes()
            .cmp(b.0.as_os_str().as_encoded_bytes())
    });
    Ok(kept)
}

/// A walk callback: the relative path, the full path, and the entry; returns whether to
/// descend into a directory.
pub(super) type Keep<'a> = dyn Fn(&Path, &Path, &Entry) -> Result<bool> + Sync + 'a;

/// Call `keep` for every entry under `root`.
#[cfg(target_os = "macos")]
fn walk(root: &Path, keep: &Keep<'_>) -> Result {
    super::bulk::walk(root, threads(), keep)
}

/// Call `keep` for every entry under `root`.
#[cfg(not(target_os = "macos"))]
fn walk(root: &Path, keep: &Keep<'_>) -> Result {
    use ignore::{WalkBuilder, WalkState};

    let failure = Mutex::new(None);
    let walker = WalkBuilder::new(root)
        .standard_filters(false)
        .follow_links(false)
        .threads(threads())
        .build_parallel();
    walker.run(|| {
        Box::new(|walked| {
            let outcome = walked
                .map_err(|error| walk_error(root, error))
                .and_then(|walked| {
                    if walked.depth() == 0 {
                        return Ok(true);
                    }
                    let path = walked.path();
                    let entry = Entry::lstat(path)?;
                    let relative = path.strip_prefix(root).map_err(|_| {
                        ForkspaceError::StateFailed(format!(
                            "{} escaped the walk of {}",
                            path.display(),
                            root.display()
                        ))
                    })?;
                    keep(relative, path, &entry)
                });
            match outcome {
                Ok(true) => WalkState::Continue,
                Ok(false) => WalkState::Skip,
                Err(error) => {
                    failure
                        .lock()
                        .expect("walk failure lock")
                        .get_or_insert(error);
                    WalkState::Quit
                }
            }
        })
    });
    match failure.into_inner().expect("walk failure lock") {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(super) fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().min(8))
        .unwrap_or(4)
}

#[cfg(not(target_os = "macos"))]
fn walk_error(root: &Path, error: ignore::Error) -> ForkspaceError {
    let source = match error.io_error() {
        Some(io) => std::io::Error::new(io.kind(), error.to_string()),
        None => std::io::Error::other(error.to_string()),
    };
    ForkspaceError::Io {
        context: format!("failed to walk {}", root.display()),
        source,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn visits_every_entry_once_in_path_order() {
        let root = tempfile::tempdir().unwrap();
        for directory in ["a/b", "c", ".hidden", "node_modules/x"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        std::fs::write(root.path().join(".gitignore"), "c\n").unwrap();
        std::fs::write(root.path().join("a/b/file"), "x").unwrap();
        std::os::unix::fs::symlink("missing", root.path().join("link")).unwrap();
        let seen = super::parallel(root.path(), |_, _, _| Ok(Some(()))).unwrap();
        let paths: Vec<String> = seen
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect();
        assert_eq!(
            paths,
            [
                ".gitignore",
                ".hidden",
                "a",
                "a/b",
                "a/b/file",
                "c",
                "link",
                "node_modules",
                "node_modules/x"
            ]
        );
    }

    #[test]
    fn entries_match_lstat() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), "12345").unwrap();
        std::fs::hard_link(root.path().join("dir/file"), root.path().join("link2")).unwrap();
        std::os::unix::fs::symlink("file", root.path().join("dir/link")).unwrap();
        let seen = super::parallel(root.path(), |_, path, entry| {
            Ok(Some((*entry, super::Entry::lstat(path)?)))
        })
        .unwrap();
        assert_eq!(seen.len(), 4);
        for (_, (walked, stat)) in seen {
            assert_eq!(walked, stat);
        }
    }
}
