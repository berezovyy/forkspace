//! Portable fork primitive: a metadata-preserving recursive copy that keeps sparse files
//! sparse and hard links linked. [`materialize_tree`] is shared with the reflink primitive,
//! which only swaps how regular files are cloned.

use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use super::xattr::copy_xattrs;
use super::{ForkPrimitive, ForkStats, ForkSummary, DIRECTORY_COPY_CAPABILITIES};
use crate::backend::{BackendKind, Capabilities};
use crate::error::{ForkspaceError, Result};
use crate::util::fs::mode_bits;
use crate::util::{IoContext, WalkDirExt};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CopyPrimitive;

impl ForkPrimitive for CopyPrimitive {
    fn kind(&self) -> BackendKind {
        BackendKind::PlainCopy
    }

    fn capabilities(&self) -> &'static Capabilities {
        &DIRECTORY_COPY_CAPABILITIES
    }

    fn check_available(&self, _base: &Path, _dest_parent: &Path) -> Result {
        Ok(())
    }

    fn materialize(&self, base: &Path, dest: &Path) -> Result<ForkStats> {
        materialize_tree(base, dest, |_source, _target| Ok(Placed::No))
    }

    fn summarize(&self, stats: &ForkStats, elapsed_ms: u128) -> ForkSummary {
        ForkSummary {
            degraded: true,
            note: Some(format!(
                "used a full copy ({} bytes in {elapsed_ms} ms); {}",
                stats.copied_bytes,
                faster_fork_remedy()
            )),
        }
    }
}

fn faster_fork_remedy() -> &'static str {
    if cfg!(target_os = "macos") {
        "place the project and FORKSPACE_HOME on the same APFS volume for faster clone forks"
    } else {
        "install fuse-overlayfs or use a reflink-capable volume for faster forks"
    }
}

/// How a `materialize_tree` callback placed a regular file at its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placed {
    /// Not placed: copy it byte for byte.
    No,
    /// Cloned (reflink); its metadata still has to be applied.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Cloned,
    /// Hard-linked to an identical immutable file, metadata included.
    Linked,
}

/// Recreates `source` at `target`, preserving permissions, mtimes, xattrs, symlinks, and hard
/// links. `place_regular` may place a regular file itself (see [`Placed`]); otherwise the file
/// is copied byte for byte.
pub(crate) fn materialize_tree<F>(
    source: &Path,
    target: &Path,
    place_regular: F,
) -> Result<ForkStats>
where
    F: FnMut(&Path, &Path) -> Result<Placed>,
{
    fs::create_dir_all(target).io_context(|| format!("create {}", target.display()))?;
    let mut copier = TreeCopier {
        place_regular,
        hardlinks: HashMap::new(),
        directories: Vec::new(),
        stats: ForkStats::default(),
    };
    let skip_sidecars = stores_xattrs_in_sidecars(source);
    for entry in WalkDir::new(source).min_depth(1).follow_links(false) {
        let entry = entry.walk_context(source)?;
        if skip_sidecars
            && entry.file_type().is_file()
            && entry.file_name().as_encoded_bytes().starts_with(b"._")
        {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| ForkspaceError::StateFailed("copy walk escaped its source".to_string()))?;
        copier.copy_entry(entry.path(), target.join(relative))?;
    }
    let stats = copier.finish_directories()?;
    sync_tree(target)?;
    Ok(stats)
}

/// Whether macOS keeps the extended attributes of files under `path` in AppleDouble `._name`
/// sidecar files, as it does on FAT, exFAT, and network volumes. The sidecars are not copied:
/// copying each file's attributes recreates them where the target needs them.
#[cfg(target_os = "macos")]
fn stores_xattrs_in_sidecars(path: &Path) -> bool {
    let Ok(name) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `name` is NUL-terminated and `stats` is a valid out-pointer for statfs.
    if unsafe { libc::statfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: statfs returned success, so it initialized `stats`.
    let stats = unsafe { stats.assume_init() };
    let kind: Vec<u8> = stats
        .f_fstypename
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    !matches!(kind.as_slice(), b"apfs" | b"hfs")
}

#[cfg(not(target_os = "macos"))]
fn stores_xattrs_in_sidecars(_path: &Path) -> bool {
    false
}

/// Make the finished copy durable with one filesystem-wide flush. An `fsync` per file costs
/// a journal commit each on ext4 (46 s instead of well under a second for 10,000 files) and
/// stalls other writers on the volume meanwhile. macOS has no `syncfs`, so it flushes all
/// filesystems; there the copy primitive is only the fallback behind APFS clones.
fn sync_tree(target: &Path) -> Result {
    #[cfg(target_os = "linux")]
    {
        let directory =
            fs::File::open(target).io_context(|| format!("open {}", target.display()))?;
        if unsafe { libc::syncfs(directory.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .io_context(|| format!("sync the filesystem holding {}", target.display()));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = target;
        unsafe { libc::sync() };
    }
    Ok(())
}

struct TreeCopier<F> {
    place_regular: F,
    /// First destination of each multiply-linked source inode, keyed by `(dev, ino)`.
    hardlinks: HashMap<(u64, u64), PathBuf>,
    /// Directories whose metadata is applied after their contents are written.
    directories: Vec<(PathBuf, PathBuf, fs::Metadata)>,
    stats: ForkStats,
}

impl<F> TreeCopier<F>
where
    F: FnMut(&Path, &Path) -> Result<Placed>,
{
    fn copy_entry(&mut self, source: &Path, destination: PathBuf) -> Result {
        self.stats.entries += 1;
        let metadata =
            fs::symlink_metadata(source).io_context(|| format!("inspect {}", source.display()))?;

        if metadata.is_dir() {
            fs::create_dir_all(&destination)
                .io_context(|| format!("create {}", destination.display()))?;
            self.directories
                .push((source.to_path_buf(), destination, metadata));
            return Ok(());
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).io_context(|| format!("create {}", parent.display()))?;
        }
        if metadata.file_type().is_symlink() {
            copy_symlink(source, &destination, &metadata)?;
        } else if metadata.is_file() {
            self.copy_file(source, destination, &metadata)?;
        } else {
            return Err(ForkspaceError::InvalidArgument(format!(
                "unsupported file kind at {}; only files, directories, and symlinks can be forked",
                source.display()
            )));
        }
        self.stats.observe(&metadata);
        Ok(())
    }

    fn copy_file(
        &mut self,
        source: &Path,
        destination: PathBuf,
        metadata: &fs::Metadata,
    ) -> Result {
        let identity = (metadata.dev(), metadata.ino());
        let multiply_linked = metadata.nlink() > 1;
        if multiply_linked {
            if let Some(existing) = self.hardlinks.get(&identity) {
                return fs::hard_link(existing, &destination)
                    .io_context(|| format!("hard-link {}", destination.display()));
            }
        }

        match (self.place_regular)(source, &destination)? {
            Placed::Linked => {
                self.stats.linked_files += 1;
                if multiply_linked {
                    self.hardlinks.insert(identity, destination);
                }
                return Ok(());
            }
            Placed::Cloned => self.stats.reflinked_files += 1,
            Placed::No => {
                copy_sparse(source, &destination, metadata.len())?;
                self.stats.copied_files += 1;
                self.stats.copied_bytes += metadata.len();
            }
        }
        // Permissions go last: a read-only mode (Git objects are 0444) would refuse the xattr
        // writes, and macOS files nearly always carry xattrs such as `com.apple.provenance`.
        copy_xattrs(source, &destination, false)?;
        set_mtime(&destination, metadata)?;
        fs::set_permissions(&destination, metadata.permissions())
            .io_context(|| format!("set permissions on {}", destination.display()))?;
        if multiply_linked {
            self.hardlinks.insert(identity, destination);
        }
        Ok(())
    }

    /// Applies directory metadata once every entry exists, deepest first, so later writes
    /// cannot bump a finished mtime and a parent's restrictive mode cannot block its children.
    fn finish_directories(mut self) -> Result<ForkStats> {
        for (source, destination, metadata) in self.directories.into_iter().rev() {
            copy_xattrs(&source, &destination, false)?;
            set_mtime(&destination, &metadata)?;
            fs::set_permissions(
                &destination,
                fs::Permissions::from_mode(mode_bits(&metadata)),
            )
            .io_context(|| format!("set permissions on {}", destination.display()))?;
            self.stats.observe(&metadata);
        }
        Ok(self.stats)
    }
}

fn copy_symlink(source: &Path, destination: &Path, metadata: &fs::Metadata) -> Result {
    let link = fs::read_link(source).io_context(|| format!("read symlink {}", source.display()))?;
    std::os::unix::fs::symlink(link, destination)
        .io_context(|| format!("create symlink {}", destination.display()))?;
    copy_xattrs(source, destination, true)?;
    // The link's own mtime: merge treats a base link whose mtime moved as re-created.
    set_symlink_mtime(destination, metadata)
}

fn set_symlink_mtime(path: &Path, metadata: &fs::Metadata) -> Result {
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| ForkspaceError::InvalidArgument("path contains a NUL byte".to_string()))?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: metadata.mtime() as libc::time_t,
            tv_nsec: metadata.mtime_nsec() as _,
        },
    ];
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .io_context(|| format!("set time on {}", path.display()));
    }
    Ok(())
}

fn set_mtime(path: &Path, metadata: &fs::Metadata) -> Result {
    let modified = metadata
        .modified()
        .io_context(|| format!("read time from {}", path.display()))?;
    fs::OpenOptions::new()
        .read(true)
        .open(path)
        .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
        .io_context(|| format!("set time on {}", path.display()))
}

/// Copies a regular file, writing only its data extents so holes stay sparse. Falls back to a
/// plain `fs::copy` when the filesystem cannot report extents.
fn copy_sparse(source: &Path, destination: &Path, length: u64) -> Result {
    let input = fs::File::open(source).io_context(|| format!("open {}", source.display()))?;
    let output = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(destination)
        .io_context(|| format!("create {}", destination.display()))?;
    output
        .set_len(length)
        .io_context(|| format!("size {}", destination.display()))?;
    if length == 0 {
        return Ok(());
    }

    if !copy_data_extents(&input, &output, source, length)? {
        drop(output);
        let _ = fs::remove_file(destination);
        fs::copy(source, destination)
            .io_context(|| format!("copy {} -> {}", source.display(), destination.display()))?;
    }
    Ok(())
}

/// Copies each `SEEK_DATA`/`SEEK_HOLE` extent of `input` into the pre-sized `output`.
/// Returns `false`, having possibly written a partial copy, when extent lookup is unsupported.
fn copy_data_extents(
    input: &fs::File,
    output: &fs::File,
    source: &Path,
    length: u64,
) -> Result<bool> {
    let fd = input.as_raw_fd();
    let mut offset = 0_u64;
    while offset < length {
        let data = unsafe { libc::lseek(fd, offset as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            // ENXIO means there is no data past `offset`: the rest of the file is a hole.
            let error = std::io::Error::last_os_error();
            return Ok(error.raw_os_error() == Some(libc::ENXIO));
        }
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(std::io::Error::last_os_error())
                .io_context(|| format!("find sparse extent in {}", source.display()));
        }
        copy_extent(input, output, data as u64, (hole as u64).min(length))?;
        offset = hole as u64;
    }
    Ok(true)
}

fn copy_extent(input: &fs::File, output: &fs::File, start: u64, end: u64) -> Result {
    let mut offset = start;
    let mut buffer = vec![0_u8; 128 * 1024];
    while offset < end {
        let want = usize::try_from((end - offset).min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = input
            .read_at(&mut buffer[..want], offset)
            .io_context(|| "read sparse file extent".to_string())?;
        if read == 0 {
            break;
        }
        output
            .write_all_at(&buffer[..read], offset)
            .io_context(|| "write sparse file extent".to_string())?;
        offset += read as u64;
    }
    Ok(())
}
