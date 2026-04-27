use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use super::super::cow::{Degradation, ForkPrimitive, ForkStats, ForkTimePolicy, ProbeResult};
use super::super::BackendKind;
use crate::error::{ForkspaceError, Result};
use crate::util::{IoContext, WalkDirExt};

#[derive(Debug, Clone, Copy, Default)]
pub struct CopyPrim;

impl ForkPrimitive for CopyPrim {
    fn kind(&self) -> BackendKind {
        BackendKind::PlainCopy
    }

    fn probe(&self, _base: &Path, _dest_parent: &Path) -> ProbeResult {
        ProbeResult::available("portable directory copy is always available")
    }

    fn materialize(&self, base: &Path, dest: &Path) -> Result<ForkStats> {
        materialize_tree(base, dest, |_source, _target| Ok(false))
    }

    fn fork_time_policy(&self) -> ForkTimePolicy {
        ForkTimePolicy::AfterMaterialize
    }

    fn degradation(&self) -> Option<Degradation> {
        Some(Degradation {
            diagnosis: "the workspace was materialized as a full copy".to_string(),
            remedy: "install fuse-overlayfs or use a reflink-capable volume for faster forks"
                .to_string(),
        })
    }
}

pub(crate) fn materialize_tree<F>(
    source: &Path,
    target: &Path,
    mut clone_regular: F,
) -> Result<ForkStats>
where
    F: FnMut(&Path, &Path) -> Result<bool>,
{
    let mut hardlinks: HashMap<(u64, u64), PathBuf> = HashMap::new();
    let mut directories = Vec::new();
    let mut stats = ForkStats::default();
    fs::create_dir_all(target).io_context(|| format!("create {}", target.display()))?;

    for entry in WalkDir::new(source).min_depth(1).follow_links(false) {
        let entry = entry.walk_context(source)?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| ForkspaceError::StateFailed("copy walk escaped its source".to_string()))?;
        let destination = target.join(relative);
        let metadata = fs::symlink_metadata(entry.path())
            .io_context(|| format!("inspect {}", entry.path().display()))?;

        if metadata.is_dir() {
            fs::create_dir_all(&destination)
                .io_context(|| format!("create {}", destination.display()))?;
            directories.push((entry.path().to_path_buf(), destination, metadata));
            continue;
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).io_context(|| format!("create {}", parent.display()))?;
        }
        if metadata.file_type().is_symlink() {
            std::os::unix::fs::symlink(
                fs::read_link(entry.path())
                    .io_context(|| format!("read symlink {}", entry.path().display()))?,
                &destination,
            )
            .io_context(|| format!("create symlink {}", destination.display()))?;
            copy_xattrs(entry.path(), &destination, true)?;
            stats.observe(&metadata);
            continue;
        }
        if !metadata.is_file() {
            return Err(ForkspaceError::InvalidArgument(format!(
                "unsupported file kind at {}; only files, directories, and symlinks can be forked",
                entry.path().display()
            )));
        }

        let identity = (metadata.dev(), metadata.ino());
        if metadata.nlink() > 1 {
            if let Some(existing) = hardlinks.get(&identity) {
                fs::hard_link(existing, &destination)
                    .io_context(|| format!("hard-link {}", destination.display()))?;
                stats.observe(&metadata);
                continue;
            }
        }

        if clone_regular(entry.path(), &destination)? {
            stats.reflinked_files += 1;
        } else {
            copy_sparse(entry.path(), &destination, metadata.len())?;
            stats.copied_files += 1;
            stats.copied_bytes += metadata.len();
        }
        fs::set_permissions(&destination, metadata.permissions())
            .io_context(|| format!("set permissions on {}", destination.display()))?;
        set_mtime(&destination, &metadata)?;
        copy_xattrs(entry.path(), &destination, false)?;
        if metadata.nlink() > 1 {
            hardlinks.insert(identity, destination);
        }
        stats.observe(&metadata);
    }

    for (source_dir, destination, metadata) in directories.into_iter().rev() {
        fs::set_permissions(
            &destination,
            fs::Permissions::from_mode(metadata.mode() & 0o7777),
        )
        .io_context(|| format!("set permissions on {}", destination.display()))?;
        set_mtime(&destination, &metadata)?;
        copy_xattrs(&source_dir, &destination, false)?;
        stats.observe(&metadata);
    }
    Ok(stats)
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

    let mut offset = 0_u64;
    let mut used_extents = false;
    while offset < length {
        let data =
            unsafe { libc::lseek(input.as_raw_fd(), offset as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                used_extents = true;
                break;
            }
            drop(output);
            let _ = fs::remove_file(destination);
            fs::copy(source, destination)
                .io_context(|| format!("copy {} -> {}", source.display(), destination.display()))?;
            return Ok(());
        }
        used_extents = true;
        let hole = unsafe { libc::lseek(input.as_raw_fd(), data, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(ForkspaceError::Io {
                context: format!("find sparse extent in {}", source.display()),
                source: std::io::Error::last_os_error(),
            });
        }
        copy_extent(&input, &output, data as u64, (hole as u64).min(length))?;
        offset = hole as u64;
    }
    if !used_extents {
        return Err(ForkspaceError::StateFailed(format!(
            "could not copy {}",
            source.display()
        )));
    }
    output
        .sync_all()
        .io_context(|| format!("sync {}", destination.display()))
}

use std::os::fd::AsRawFd;

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
        let mut written = 0;
        while written < read {
            written += output
                .write_at(&buffer[written..read], offset + written as u64)
                .io_context(|| "write sparse file extent".to_string())?;
        }
        offset += read as u64;
    }
    Ok(())
}

fn copy_xattrs(source: &Path, destination: &Path, symlink: bool) -> Result {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| ForkspaceError::InvalidArgument("path contains a NUL byte".to_string()))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| ForkspaceError::InvalidArgument("path contains a NUL byte".to_string()))?;
    let size = list_xattrs(&source, std::ptr::null_mut(), 0, symlink);
    if size < 0 {
        if xattrs_unsupported() {
            return Ok(());
        }
        return xattr_error("list", &source);
    }
    if size == 0 {
        return Ok(());
    }
    let mut names = vec![0_u8; size as usize];
    let size = list_xattrs(&source, names.as_mut_ptr().cast(), names.len(), symlink);
    if size < 0 {
        if xattrs_unsupported() {
            return Ok(());
        }
        return xattr_error("list", &source);
    }
    names.truncate(size as usize);
    for raw_name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = CString::new(raw_name)
            .map_err(|_| ForkspaceError::StateFailed("xattr name contains NUL".to_string()))?;
        let size = get_xattr(&source, &name, std::ptr::null_mut(), 0, symlink);
        if size < 0 {
            return xattr_error("read", &source);
        }
        let mut value = vec![0_u8; size as usize];
        if size > 0
            && get_xattr(
                &source,
                &name,
                value.as_mut_ptr().cast(),
                value.len(),
                symlink,
            ) < 0
        {
            return xattr_error("read", &source);
        }
        if set_xattr(
            &destination,
            &name,
            value.as_ptr().cast(),
            value.len(),
            symlink,
        ) < 0
        {
            if xattrs_unsupported() {
                continue;
            }
            return xattr_error("write", &destination);
        }
    }
    Ok(())
}

fn xattr_error<T>(operation: &str, path: &CStr) -> Result<T> {
    let error = std::io::Error::last_os_error();
    Err(ForkspaceError::Io {
        context: format!(
            "{operation} extended attributes on {}",
            path.to_string_lossy()
        ),
        source: error,
    })
}

fn xattrs_unsupported() -> bool {
    let code = std::io::Error::last_os_error().raw_os_error();
    code == Some(libc::ENOTSUP) || code == Some(libc::EOPNOTSUPP)
}

#[cfg(target_os = "linux")]
fn list_xattrs(path: &CStr, list: *mut libc::c_char, size: usize, symlink: bool) -> isize {
    unsafe {
        if symlink {
            libc::llistxattr(path.as_ptr(), list, size)
        } else {
            libc::listxattr(path.as_ptr(), list, size)
        }
    }
}

#[cfg(target_os = "macos")]
fn list_xattrs(path: &CStr, list: *mut libc::c_char, size: usize, symlink: bool) -> isize {
    let options = if symlink { libc::XATTR_NOFOLLOW } else { 0 };
    unsafe { libc::listxattr(path.as_ptr(), list, size, options) }
}

#[cfg(target_os = "linux")]
fn get_xattr(
    path: &CStr,
    name: &CStr,
    value: *mut libc::c_void,
    size: usize,
    symlink: bool,
) -> isize {
    unsafe {
        if symlink {
            libc::lgetxattr(path.as_ptr(), name.as_ptr(), value, size)
        } else {
            libc::getxattr(path.as_ptr(), name.as_ptr(), value, size)
        }
    }
}

#[cfg(target_os = "macos")]
fn get_xattr(
    path: &CStr,
    name: &CStr,
    value: *mut libc::c_void,
    size: usize,
    symlink: bool,
) -> isize {
    let options = if symlink { libc::XATTR_NOFOLLOW } else { 0 };
    unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), value, size, 0, options) }
}

#[cfg(target_os = "linux")]
fn set_xattr(
    path: &CStr,
    name: &CStr,
    value: *const libc::c_void,
    size: usize,
    symlink: bool,
) -> libc::c_int {
    unsafe {
        if symlink {
            libc::lsetxattr(path.as_ptr(), name.as_ptr(), value, size, 0)
        } else {
            libc::setxattr(path.as_ptr(), name.as_ptr(), value, size, 0)
        }
    }
}

#[cfg(target_os = "macos")]
fn set_xattr(
    path: &CStr,
    name: &CStr,
    value: *const libc::c_void,
    size: usize,
    symlink: bool,
) -> libc::c_int {
    let options = if symlink { libc::XATTR_NOFOLLOW } else { 0 };
    unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), value, size, 0, options) }
}
