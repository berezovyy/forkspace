//! Extended-attribute copying for the copy and reflink primitives. Filesystems without xattr
//! support are skipped silently; any other failure is an error.

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::error::{ForkspaceError, Result};
use crate::util::IoContext;

/// Copies every extended attribute of `source` onto `destination`. With `symlink`, the
/// attributes of the links themselves are copied rather than of their targets.
pub(super) fn copy_xattrs(source: &Path, destination: &Path, symlink: bool) -> Result {
    let source = c_path(source)?;
    let destination = c_path(destination)?;
    let names = list_names(&source, symlink)?;
    for raw_name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = CString::new(raw_name)
            .map_err(|_| ForkspaceError::StateFailed("xattr name contains NUL".to_string()))?;
        let value = read_value(&source, &name, symlink)?;
        if sys::set(&destination, &name, &value, symlink) < 0 && !unsupported() {
            return os_error("write", &destination);
        }
    }
    Ok(())
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ForkspaceError::InvalidArgument("path contains a NUL byte".to_string()))
}

/// The NUL-separated attribute names of `path`; empty when the filesystem has no xattrs.
fn list_names(path: &CStr, symlink: bool) -> Result<Vec<u8>> {
    let size = list_len(sys::list(path, &mut [], symlink), path)?;
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut names = vec![0_u8; size];
    let size = list_len(sys::list(path, &mut names, symlink), path)?;
    names.truncate(size);
    Ok(names)
}

/// Converts a `listxattr` return value to a length, treating "unsupported" as no attributes.
fn list_len(result: isize, path: &CStr) -> Result<usize> {
    if result >= 0 {
        return Ok(result as usize);
    }
    if unsupported() {
        Ok(0)
    } else {
        os_error("list", path)
    }
}

fn read_value(path: &CStr, name: &CStr, symlink: bool) -> Result<Vec<u8>> {
    let size = sys::get(path, name, &mut [], symlink);
    if size < 0 {
        return os_error("read", path);
    }
    let mut value = vec![0_u8; size as usize];
    if size > 0 && sys::get(path, name, &mut value, symlink) < 0 {
        return os_error("read", path);
    }
    Ok(value)
}

/// Whether the last OS error means the filesystem does not support xattrs.
fn unsupported() -> bool {
    let code = std::io::Error::last_os_error().raw_os_error();
    code == Some(libc::ENOTSUP) || code == Some(libc::EOPNOTSUPP)
}

fn os_error<T>(operation: &str, path: &CStr) -> Result<T> {
    Err(std::io::Error::last_os_error()).io_context(|| {
        format!(
            "{operation} extended attributes on {}",
            path.to_string_lossy()
        )
    })
}

/// Output pointer for an xattr call; NULL for an empty buffer, which asks for the size only.
fn out_ptr(buffer: &mut [u8]) -> *mut libc::c_void {
    if buffer.is_empty() {
        std::ptr::null_mut()
    } else {
        buffer.as_mut_ptr().cast()
    }
}

/// Thin wrappers over the platform xattr calls. An empty buffer queries the required size.
#[cfg(target_os = "linux")]
mod sys {
    use std::ffi::CStr;

    use super::out_ptr;

    pub(super) fn list(path: &CStr, buffer: &mut [u8], symlink: bool) -> isize {
        let (list, size) = (out_ptr(buffer).cast(), buffer.len());
        unsafe {
            if symlink {
                libc::llistxattr(path.as_ptr(), list, size)
            } else {
                libc::listxattr(path.as_ptr(), list, size)
            }
        }
    }

    pub(super) fn get(path: &CStr, name: &CStr, buffer: &mut [u8], symlink: bool) -> isize {
        let (value, size) = (out_ptr(buffer), buffer.len());
        unsafe {
            if symlink {
                libc::lgetxattr(path.as_ptr(), name.as_ptr(), value, size)
            } else {
                libc::getxattr(path.as_ptr(), name.as_ptr(), value, size)
            }
        }
    }

    pub(super) fn set(path: &CStr, name: &CStr, value: &[u8], symlink: bool) -> libc::c_int {
        let (data, size) = (value.as_ptr().cast(), value.len());
        unsafe {
            if symlink {
                libc::lsetxattr(path.as_ptr(), name.as_ptr(), data, size, 0)
            } else {
                libc::setxattr(path.as_ptr(), name.as_ptr(), data, size, 0)
            }
        }
    }
}

/// Thin wrappers over the platform xattr calls. An empty buffer queries the required size.
#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::CStr;

    use super::out_ptr;

    fn options(symlink: bool) -> libc::c_int {
        if symlink {
            libc::XATTR_NOFOLLOW
        } else {
            0
        }
    }

    pub(super) fn list(path: &CStr, buffer: &mut [u8], symlink: bool) -> isize {
        let (list, size) = (out_ptr(buffer).cast(), buffer.len());
        unsafe { libc::listxattr(path.as_ptr(), list, size, options(symlink)) }
    }

    pub(super) fn get(path: &CStr, name: &CStr, buffer: &mut [u8], symlink: bool) -> isize {
        let (value, size) = (out_ptr(buffer), buffer.len());
        unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value,
                size,
                0,
                options(symlink),
            )
        }
    }

    pub(super) fn set(path: &CStr, name: &CStr, value: &[u8], symlink: bool) -> libc::c_int {
        let (data, size) = (value.as_ptr().cast(), value.len());
        unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                data,
                size,
                0,
                options(symlink),
            )
        }
    }
}
