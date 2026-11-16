//! Portable fork primitive: a metadata-preserving recursive copy that keeps sparse files
//! sparse and hard links linked. [`materialize_tree`] is shared with the reflink primitive,
//! which only swaps how regular files are cloned.

use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use super::xattr::copy_xattrs;
use super::{ForkPrimitive, ForkStats, ForkSummary, DIRECTORY_COPY_CAPABILITIES};
use crate::backend::{BackendKind, Capabilities};
use crate::error::{ForkspaceError, Result};
use crate::util::walk::Entry;
use crate::util::IoContext;

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
        materialize_tree(base, dest, |_, _, _| Ok(Placed::No))
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
/// links. `place_regular(source, target, facts)` may place a regular file itself (see
/// [`Placed`]); otherwise the file is copied byte for byte.
///
/// One walk creates the directories and symlinks and lists the regular files. The files are
/// then placed on several threads: copying a dependency tree is dominated by per-file system
/// calls, which a single thread runs one after another. A file with several links is placed
/// once and its other names are linked to that copy afterwards. Directory metadata goes last.
pub(crate) fn materialize_tree<F>(
    source: &Path,
    target: &Path,
    place_regular: F,
) -> Result<ForkStats>
where
    F: Fn(&Path, &Path, &Entry) -> Result<Placed> + Sync,
{
    let (stats, _) = materialize(source, target, &place_regular, false, true)?;
    Ok(stats)
}

/// [`materialize_tree`] that also returns every entry it created, relative to `target`, with
/// the facts of the copy (a file's size, mtime and mode are its source's; a directory's are
/// read back once it is finished), so the caller can describe the tree without walking it.
/// With `durable: false` the copy is not flushed to disk; the caller takes care of that.
pub(crate) fn materialize_recorded<F>(
    source: &Path,
    target: &Path,
    place_regular: F,
    durable: bool,
) -> Result<(ForkStats, Vec<(PathBuf, Entry)>)>
where
    F: Fn(&Path, &Path, &Entry) -> Result<Placed> + Sync,
{
    materialize(source, target, &place_regular, true, durable)
}

fn materialize<F>(
    source: &Path,
    target: &Path,
    place_regular: &F,
    record: bool,
    durable: bool,
) -> Result<(ForkStats, Vec<(PathBuf, Entry)>)>
where
    F: Fn(&Path, &Path, &Entry) -> Result<Placed> + Sync,
{
    fs::create_dir_all(target).io_context(|| format!("create {}", target.display()))?;
    let mut plan = plan_tree(source, target, record)?;
    let mut stats = plan.stats;
    stats.absorb(place_files(&plan.files, place_regular)?);
    for (existing, destination) in &plan.links {
        fs::hard_link(existing, destination)
            .io_context(|| format!("hard-link {}", destination.display()))?;
    }
    finish_directories(plan.directories, &mut stats, &mut plan.entries)?;
    if durable {
        sync_tree(target)?;
    }
    Ok((stats, plan.entries))
}

/// What [`plan_tree`] leaves for the file placement and the final pass.
struct TreePlan {
    /// Regular files to place, one per inode.
    files: Vec<FileJob>,
    /// Further names of multiply-linked files: `(first destination, this destination)`.
    links: Vec<(PathBuf, PathBuf)>,
    /// Directories whose metadata is applied after their contents are written, with the index
    /// of their recorded entry.
    directories: Vec<(PathBuf, PathBuf, Entry, Option<usize>)>,
    /// With recording on, every entry relative to the target, in walk order.
    entries: Vec<(PathBuf, Entry)>,
    stats: ForkStats,
}

struct FileJob {
    source: PathBuf,
    destination: PathBuf,
    entry: Entry,
}

/// Walk `source`, creating every directory and symlink under `target` and listing the
/// regular files to place.
fn plan_tree(source: &Path, target: &Path, record: bool) -> Result<TreePlan> {
    let mut plan = TreePlan {
        files: Vec::new(),
        links: Vec::new(),
        directories: Vec::new(),
        entries: Vec::new(),
        stats: ForkStats::default(),
    };
    // First destination of each multiply-linked source inode, keyed by `(dev, ino)`.
    let mut first_link: HashMap<(u64, u64), PathBuf> = HashMap::new();
    let skip_sidecars = stores_xattrs_in_sidecars(source);
    // The tree is read on several threads; entries come back in path order, so a directory
    // always comes before what it contains.
    let walked = crate::util::walk::parallel(source, |relative, _, entry| {
        let sidecar = skip_sidecars
            && entry.is_file()
            && relative
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"._"));
        Ok((!sidecar).then_some(*entry))
    })?;
    for (relative, entry) in walked {
        let source = source.join(&relative);
        let destination = target.join(&relative);
        plan.stats.entries += 1;
        let recorded = record.then(|| {
            plan.entries.push((relative, entry));
            plan.entries.len() - 1
        });

        if entry.is_dir() {
            fs::create_dir_all(&destination)
                .io_context(|| format!("create {}", destination.display()))?;
            plan.directories
                .push((source, destination, entry, recorded));
            continue;
        }
        plan.stats.observe_entry(&entry);
        if entry.is_symlink() {
            copy_symlink(&source, &destination, &entry)?;
        } else if entry.is_file() {
            if entry.nlink > 1 {
                let identity = (entry.dev, entry.ino);
                if let Some(existing) = first_link.get(&identity) {
                    plan.links.push((existing.clone(), destination));
                    continue;
                }
                first_link.insert(identity, destination.clone());
            }
            plan.files.push(FileJob {
                source,
                destination,
                entry,
            });
        } else {
            return Err(ForkspaceError::InvalidArgument(format!(
                "unsupported file kind at {}; only files, directories, and symlinks can be forked",
                source.display()
            )));
        }
    }
    Ok(plan)
}

/// Files a thread takes on before another is worth starting.
const FILES_PER_THREAD: usize = 256;

/// Place every file of `files` on up to [`crate::util::walk::threads`] threads. The first
/// error stops the others and is returned.
fn place_files<F>(files: &[FileJob], place_regular: &F) -> Result<ForkStats>
where
    F: Fn(&Path, &Path, &Entry) -> Result<Placed> + Sync,
{
    let threads = crate::util::walk::threads()
        .min(files.len().div_ceil(FILES_PER_THREAD))
        .max(1);
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let failure = Mutex::new(None);
    let work = || {
        let mut stats = ForkStats::default();
        while !stop.load(Ordering::Relaxed) {
            let Some(job) = files.get(next.fetch_add(1, Ordering::Relaxed)) else {
                break;
            };
            if let Err(error) = place_file(job, place_regular, &mut stats) {
                stop.store(true, Ordering::Relaxed);
                failure
                    .lock()
                    .expect("file placement failure")
                    .get_or_insert(error);
                break;
            }
        }
        stats
    };
    let mut stats = ForkStats::default();
    if threads == 1 {
        stats = work();
    } else {
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads).map(|_| scope.spawn(work)).collect();
            for worker in workers {
                stats.absorb(worker.join().expect("file placement thread"));
            }
        });
    }
    match failure.into_inner().expect("file placement failure") {
        Some(error) => Err(error),
        None => Ok(stats),
    }
}

/// Place one regular file and apply its metadata.
fn place_file<F>(job: &FileJob, place_regular: &F, stats: &mut ForkStats) -> Result
where
    F: Fn(&Path, &Path, &Entry) -> Result<Placed>,
{
    let FileJob {
        source,
        destination,
        entry,
    } = job;
    match place_regular(source, destination, entry)? {
        Placed::Linked => {
            stats.linked_files += 1;
            return Ok(());
        }
        Placed::Cloned => stats.reflinked_files += 1,
        Placed::No => {
            copy_sparse(source, destination, entry.len)?;
            stats.copied_files += 1;
            stats.copied_bytes += entry.len;
        }
    }
    // Permissions go last: a read-only mode (Git objects are 0444) would refuse the xattr
    // writes, and macOS files nearly always carry xattrs such as `com.apple.provenance`.
    copy_xattrs(source, destination, false)?;
    set_mtime(destination, entry, false)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(entry.mode))
        .io_context(|| format!("set permissions on {}", destination.display()))
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

/// Applies directory metadata once every entry exists, deepest first, so later writes cannot
/// bump a finished mtime and a parent's restrictive mode cannot block its children.
fn finish_directories(
    directories: Vec<(PathBuf, PathBuf, Entry, Option<usize>)>,
    stats: &mut ForkStats,
    entries: &mut [(PathBuf, Entry)],
) -> Result {
    for (source, destination, entry, recorded) in directories.into_iter().rev() {
        copy_xattrs(&source, &destination, false)?;
        set_mtime(&destination, &entry, false)?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(entry.mode))
            .io_context(|| format!("set permissions on {}", destination.display()))?;
        stats.observe_entry(&entry);
        // A directory's size depends on how it was filled, so the copy's own is recorded.
        if let Some(index) = recorded {
            entries[index].1 = Entry::lstat(&destination)?;
        }
    }
    Ok(())
}

fn copy_symlink(source: &Path, destination: &Path, entry: &Entry) -> Result {
    let link = fs::read_link(source).io_context(|| format!("read symlink {}", source.display()))?;
    std::os::unix::fs::symlink(link, destination)
        .io_context(|| format!("create symlink {}", destination.display()))?;
    copy_xattrs(source, destination, true)?;
    // The link's own mtime: merge treats a base link whose mtime moved as re-created.
    set_mtime(destination, entry, true)
}

/// Set the modification time of `path` (of the link itself with `symlink`) to `entry`'s, in one
/// `utimensat` call, leaving the access time alone.
fn set_mtime(path: &Path, entry: &Entry, symlink: bool) -> Result {
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| ForkspaceError::InvalidArgument("path contains a NUL byte".to_string()))?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: entry.mtime_ns.div_euclid(1_000_000_000) as libc::time_t,
            tv_nsec: entry.mtime_ns.rem_euclid(1_000_000_000) as _,
        },
    ];
    let flags = if symlink {
        libc::AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    if unsafe { libc::utimensat(libc::AT_FDCWD, name.as_ptr(), times.as_ptr(), flags) } != 0 {
        return Err(std::io::Error::last_os_error())
            .io_context(|| format!("set time on {}", path.display()));
    }
    Ok(())
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
    #[cfg(target_os = "linux")]
    if copy_extent_in_kernel(input, output, start, end)? {
        return Ok(());
    }
    copy_extent_through_buffer(input, output, start, end)
}

/// Copy `start..end` with `copy_file_range`, so the data never passes through this process.
/// Returns `false` (having copied nothing) when the kernel or filesystem cannot do it.
#[cfg(target_os = "linux")]
fn copy_extent_in_kernel(
    input: &fs::File,
    output: &fs::File,
    start: u64,
    end: u64,
) -> Result<bool> {
    let mut offset = start as libc::loff_t;
    let end = end as libc::loff_t;
    while offset < end {
        let mut from = offset;
        let mut to = offset;
        let want = usize::try_from(end - offset).unwrap_or(usize::MAX);
        // SAFETY: both descriptors are open for the call, and the offsets are valid pointers
        // to locals the kernel updates.
        let copied = unsafe {
            libc::copy_file_range(
                input.as_raw_fd(),
                &mut from,
                output.as_raw_fd(),
                &mut to,
                want,
                0,
            )
        };
        if copied < 0 {
            let error = std::io::Error::last_os_error();
            let unsupported = matches!(
                error.raw_os_error(),
                Some(libc::ENOSYS | libc::EXDEV | libc::EINVAL | libc::EOPNOTSUPP)
            );
            if unsupported && offset == start as libc::loff_t {
                return Ok(false);
            }
            return Err(error).io_context(|| "copy file extent".to_string());
        }
        if copied == 0 {
            break;
        }
        offset += copied as libc::loff_t;
    }
    Ok(true)
}

fn copy_extent_through_buffer(input: &fs::File, output: &fs::File, start: u64, end: u64) -> Result {
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
