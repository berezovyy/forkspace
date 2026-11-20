//! Filesystem helpers: error context, durable writes, and metadata timestamps.

use std::fs;
use std::io::{Read, Write};
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

/// Create `path`'s parent directory (and its ancestors) if missing.
pub fn ensure_parent_dir(path: &Path) -> Result {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .io_context(|| format!("create parent directory {}", parent.display()))?;
    }
    Ok(())
}

/// Create `path` (and its ancestors) if missing, and make `path` itself owner-only (0700).
///
/// Workspaces hold copies of ignored files such as `.env`, so the directory above them must
/// not be traversable by other users whatever the umask was. An existing directory with
/// group or other bits is tightened in place.
pub fn ensure_private_dir(path: &Path) -> Result {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if !path.is_dir() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .io_context(|| format!("create parent directory {}", parent.display()))?;
        }
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(error).io_context(|| format!("create {}", path.display()));
            }
            _ => {}
        }
    }
    let metadata = fs::metadata(path).io_context(|| format!("inspect {}", path.display()))?;
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .io_context(|| format!("make {} private", path.display()))?;
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

/// A clock reading, in nanoseconds since the epoch, that separates writes by ctime: every
/// entry written after this call has a later ctime, and every entry written before it a ctime
/// no later than the reading.
///
/// Linux stamps inode times from its coarse clock, which lags the fine clock by up to a tick
/// (1–10 ms), or, since 6.13 on most local filesystems, from the fine clock when the previous
/// stamp was read ("multigrain" timestamps). So neither clock alone separates the two: a
/// write right after a fine reading can carry an earlier coarse stamp, and a write just
/// before a coarse reading a later fine one. The call reads the fine clock, then waits until
/// the coarse clock has passed that reading (about a tick) and returns the coarse reading
/// less a nanosecond: an earlier write is stamped at most the fine reading, a later one at
/// least the coarse reading, by either clock (0 of 1,500 writes either way on tmpfs and ext4,
/// kernel 7.0, 2026-11-19). APFS stamps from the fine clock: a ctime was never earlier than
/// the clock read just before the write (n = 2,000, 2026-11-19).
pub fn ctime_clock_ns() -> u128 {
    #[cfg(target_os = "linux")]
    {
        let fine = crate::util::now_unix_ns();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        loop {
            match coarse_clock_ns() {
                Some(coarse) if coarse > fine => return coarse - 1,
                Some(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
                // A coarse clock that is missing or does not move: the fine reading is the
                // best bound left.
                _ => return fine,
            }
        }
    }
    #[allow(unreachable_code)]
    crate::util::now_unix_ns()
}

/// The coarse realtime clock Linux stamps inode times from.
#[cfg(target_os = "linux")]
fn coarse_clock_ns() -> Option<u128> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid out-pointer for clock_gettime.
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut now) } != 0 {
        return None;
    }
    let seconds = u128::try_from(now.tv_sec).ok()?;
    let nanos = u128::try_from(now.tv_nsec).ok()?;
    Some(seconds * 1_000_000_000 + nanos)
}

/// Permission bits (including setuid/setgid/sticky).
pub fn mode_bits(metadata: &fs::Metadata) -> u32 {
    metadata.mode() & 0o7777
}

/// Whether `root` holds `relative` spelled exactly so, every component byte for byte. On a
/// case- or normalization-insensitive volume (APFS) `Path::exists` also finds `foo.txt` when
/// only `Foo.txt` is there.
pub fn exists_exact(root: &Path, relative: &Path) -> bool {
    if fs::symlink_metadata(root.join(relative)).is_err() {
        return false;
    }
    let mut directory = root.to_path_buf();
    relative.components().all(|component| {
        let name = component.as_os_str();
        let listed = fs::read_dir(&directory).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().as_encoded_bytes() == name.as_encoded_bytes())
        });
        directory.push(name);
        listed
    })
}

/// Whether two files have identical bytes. Unreadable files never compare equal.
pub fn same_contents(a: &Path, b: &Path) -> bool {
    let (Ok(mut file_a), Ok(mut file_b)) = (fs::File::open(a), fs::File::open(b)) else {
        return false;
    };
    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];
    loop {
        let (Ok(n_a), Ok(n_b)) = (file_a.read(&mut buf_a), file_b.read(&mut buf_b)) else {
            return false;
        };
        if buf_a[..n_a] != buf_b[..n_b] {
            return false;
        }
        if n_a == 0 {
            return true;
        }
    }
}

/// What an atomic rename flag did.
#[derive(Debug, PartialEq, Eq)]
pub enum FlaggedRename {
    Done,
    /// The destination already existed (`rename_noreplace` only); nothing moved.
    DestinationExists,
    /// The filesystem or kernel does not support the flag; nothing moved.
    Unsupported,
}

/// Move `from` to `to` only if `to` does not exist, atomically (`renameat2(RENAME_NOREPLACE)`
/// on Linux, `renamex_np(RENAME_EXCL)` on macOS).
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<FlaggedRename> {
    flagged_rename(from, to, RenameFlag::NoReplace)
}

/// Swap `a` and `b` atomically; both must exist and may be of different types
/// (`renameat2(RENAME_EXCHANGE)` on Linux, `renamex_np(RENAME_SWAP)` on macOS).
pub fn rename_exchange(a: &Path, b: &Path) -> std::io::Result<FlaggedRename> {
    flagged_rename(a, b, RenameFlag::Exchange)
}

/// Whether a directory at `path` can be swapped with [`rename_exchange`] and still be used
/// afterwards. fuse-overlayfs accepts the swap of a directory with entries in its lower layer,
/// but the swapped directory then lists entries it cannot open, so it can no longer be
/// removed (PULL-18). Kernel overlayfs is treated the same (not measured).
#[cfg(target_os = "linux")]
pub fn exchanges_directories(path: &Path) -> bool {
    const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;
    const OVERLAYFS_SUPER_MAGIC: u64 = 0x794c_7630;
    let Ok(name) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `name` is NUL-terminated and `stats` is a valid out-pointer for statfs.
    if unsafe { libc::statfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: statfs returned success, so it initialized `stats`.
    #[allow(clippy::unnecessary_cast)]
    let kind = unsafe { stats.assume_init() }.f_type as u64;
    kind != FUSE_SUPER_MAGIC && kind != OVERLAYFS_SUPER_MAGIC
}

#[cfg(not(target_os = "linux"))]
pub fn exchanges_directories(_path: &Path) -> bool {
    true
}

#[derive(Clone, Copy)]
enum RenameFlag {
    NoReplace,
    Exchange,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn flagged_rename(from: &Path, to: &Path, flag: RenameFlag) -> std::io::Result<FlaggedRename> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = |path: &Path| {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))
    };
    let (from, to) = (c_path(from)?, c_path(to)?);
    if raw_flagged_rename(&from, &to, flag) == 0 {
        return Ok(FlaggedRename::Done);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EEXIST) => Ok(FlaggedRename::DestinationExists),
        Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP) => Ok(FlaggedRename::Unsupported),
        #[allow(unreachable_patterns)]
        Some(libc::EOPNOTSUPP) => Ok(FlaggedRename::Unsupported),
        _ => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn raw_flagged_rename(from: &std::ffi::CStr, to: &std::ffi::CStr, flag: RenameFlag) -> i64 {
    let flags = match flag {
        RenameFlag::NoReplace => libc::RENAME_NOREPLACE,
        RenameFlag::Exchange => libc::RENAME_EXCHANGE,
    };
    // SAFETY: both paths are NUL-terminated and outlive the call.
    unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flags,
        )
    }
}

#[cfg(target_os = "macos")]
fn raw_flagged_rename(from: &std::ffi::CStr, to: &std::ffi::CStr, flag: RenameFlag) -> i64 {
    let flags = match flag {
        RenameFlag::NoReplace => libc::RENAME_EXCL,
        RenameFlag::Exchange => libc::RENAME_SWAP,
    };
    // SAFETY: both paths are NUL-terminated and outlive the call.
    i64::from(unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), flags) })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn flagged_rename(_from: &Path, _to: &Path, _flag: RenameFlag) -> std::io::Result<FlaggedRename> {
    Ok(FlaggedRename::Unsupported)
}

/// The type of the filesystem holding `path` (`ext4`, `tmpfs`, ...), for messages.
/// Used only to word messages: what a filesystem can do is always probed, never inferred
/// from its type.
#[cfg(target_os = "linux")]
pub fn filesystem_name(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return "this filesystem".to_string();
    };
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `info` is a valid out-pointer.
    if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return "this filesystem".to_string();
    }
    // SAFETY: statfs filled `info`.
    let kind = unsafe { info.assume_init() }.f_type as u64;
    let name = match kind {
        0xEF53 => "ext4",
        0x5846_5342 => "XFS",
        0x9123_683E => "Btrfs",
        0x0102_1994 => "tmpfs",
        0x794C_7630 => "overlayfs",
        0x6573_5546 => "a FUSE filesystem",
        0x6969 => "NFS",
        0x2FC1_2FC1 => "ZFS",
        0xF2F5_2010 => "F2FS",
        0x4D44 => "FAT",
        0x2011_BAB0 => "exFAT",
        0x5346_544E => "NTFS",
        0xCA45_1A4E => "bcachefs",
        _ => return format!("this filesystem (type {kind:#x})"),
    };
    name.to_string()
}
