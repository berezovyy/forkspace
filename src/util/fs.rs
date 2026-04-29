//! Filesystem helpers: error context, durable writes, and metadata timestamps.

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::error::{ForkspaceError, Result};

/// Attach a human-readable context to an `io::Result`.
pub trait IoContext<T> {
    fn io_context(self, ctx: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn io_context(self, ctx: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| ForkspaceError::Io {
            context: ctx(),
            source,
        })
    }
}

/// Convert a `walkdir` entry error into a contextual [`ForkspaceError::Io`].
pub trait WalkDirExt {
    fn walk_context(self, dir: &Path) -> Result<walkdir::DirEntry>;
}

impl WalkDirExt for std::result::Result<walkdir::DirEntry, walkdir::Error> {
    fn walk_context(self, dir: &Path) -> Result<walkdir::DirEntry> {
        self.map_err(|e| ForkspaceError::Io {
            context: format!("failed to walk {}", dir.display()),
            source: e
                .into_io_error()
                .unwrap_or_else(|| std::io::Error::other("directory walk error")),
        })
    }
}

pub fn ensure_parent_dir(path: &Path) -> Result {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .io_context(|| format!("create parent directory {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) static PARENT_DIRECTORY_SYNCED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// fsync a directory so a rename inside it is durable.
pub fn sync_dir(path: &Path) -> Result {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .io_context(|| format!("sync directory {}", path.display()))?;
    #[cfg(test)]
    PARENT_DIRECTORY_SYNCED.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// fsync a file by path.
pub fn sync_file(path: &Path) -> Result {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .io_context(|| format!("sync {}", path.display()))
}

/// Crash-safe replace: write `<path>.tmp`, fsync it, rename over `path`, fsync the directory.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory).io_context(|| format!("create {}", directory.display()))?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = Path::new(&temporary);

    let mut file =
        fs::File::create(temporary).io_context(|| format!("create {}", temporary.display()))?;
    file.write_all(bytes)
        .io_context(|| format!("write {}", temporary.display()))?;
    file.sync_all()
        .io_context(|| format!("sync {}", temporary.display()))?;
    fs::rename(temporary, path).io_context(|| format!("install {}", path.display()))?;
    sync_dir(directory)
}

/// Modification time in nanoseconds since the epoch.
pub fn mtime_ns(metadata: &fs::Metadata) -> i128 {
    i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
}

/// Status-change time in nanoseconds since the epoch.
pub fn ctime_ns(metadata: &fs::Metadata) -> i128 {
    i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec())
}

/// Permission bits (including setuid/setgid/sticky).
pub fn mode_bits(metadata: &fs::Metadata) -> u32 {
    metadata.mode() & 0o7777
}
