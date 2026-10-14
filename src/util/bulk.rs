//! macOS directory walks with `getattrlistbulk(2)`: one call returns the names and
//! attributes of many entries in a directory, instead of one `lstat` per entry.
//!
//! Directories are still `lstat`ed, because the bulk attributes do not include a directory's
//! `st_size`. Before the bulk results are trusted, the first directory read in a process is
//! checked entry by entry against `lstat`; any difference (or any failing call, or
//! `FORKSPACE_NO_BULK`) switches the process to `readdir` and `lstat` for good, so a surprise in
//! the attribute layout costs speed, never correctness.

#![cfg(target_os = "macos")]

use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Condvar, Mutex};

use super::walk::{Entry, Keep, Kind};
use crate::error::{ForkspaceError, Result};

/// `ATTR_CMN_ERROR` from `sys/attr.h` (not in the libc crate).
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
/// `enum vtype` values from `sys/vnode.h`.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;
const BUFFER_BYTES: usize = 256 * 1024;

const UNKNOWN: u8 = 0;
const TRUSTED: u8 = 1;
const DISTRUSTED: u8 = 2;
static BULK: AtomicU8 = AtomicU8::new(UNKNOWN);

/// Walk `root` on `threads` threads, calling `keep` for every entry below it.
pub(super) fn walk(root: &Path, threads: usize, keep: &Keep<'_>) -> Result {
    if std::env::var_os("FORKSPACE_NO_BULK").is_some() {
        BULK.store(DISTRUSTED, Ordering::Relaxed);
    }
    run(root, threads, &|directory| {
        visit_directory(root, directory, keep)
    })
}

/// The bytes of the tree below `root` that no clone shares (`ATTR_CMNEXT_PRIVATESIZE` of each
/// file; allocated bytes for other entries), read a directory at a time instead of one
/// `getattrlist` per file. Hard links count once. `None` if any read fails or bulk reads are
/// off, so the caller can measure file by file instead.
pub(crate) fn private_tree_bytes(root: &Path, threads: usize) -> Option<u64> {
    if std::env::var_os("FORKSPACE_NO_BULK").is_some() || BULK.load(Ordering::Relaxed) == DISTRUSTED
    {
        return None;
    }
    let total = std::sync::atomic::AtomicU64::new(0);
    let linked = Mutex::new(std::collections::HashSet::new());
    let visit = |directory: &Path| -> Result<Vec<PathBuf>> {
        let unreadable =
            || ForkspaceError::StateFailed(format!("bulk read of {}", directory.display()));
        let mut descend = Vec::new();
        for sized in private_sizes(directory).ok_or_else(unreadable)? {
            if sized.dir {
                descend.push(directory.join(&sized.name));
                continue;
            }
            if sized.nlink > 1 && !linked.lock().expect("hard link set").insert(sized.ino) {
                continue;
            }
            total.fetch_add(sized.bytes, Ordering::Relaxed);
        }
        Ok(descend)
    };
    run(root, threads, &visit).ok()?;
    Some(total.into_inner())
}

/// Visit `root` and every directory `visit` returns, on `threads` threads; `visit` reads one
/// directory and returns its subdirectories to visit next. The first error stops the walk.
fn run(
    root: &Path,
    threads: usize,
    visit: &(dyn Fn(&Path) -> Result<Vec<PathBuf>> + Sync),
) -> Result {
    let queue = Queue {
        state: Mutex::new(QueueState {
            pending: vec![root.to_path_buf()],
            busy: 0,
            failure: None,
        }),
        ready: Condvar::new(),
    };
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| worker(&queue, visit));
        }
    });
    match queue.state.into_inner().expect("walk queue").failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

struct QueueState {
    pending: Vec<PathBuf>,
    busy: usize,
    failure: Option<ForkspaceError>,
}

fn worker(queue: &Queue, visit: &(dyn Fn(&Path) -> Result<Vec<PathBuf>> + Sync)) {
    loop {
        let directory = {
            let mut state = queue.state.lock().expect("walk queue");
            loop {
                if state.failure.is_some() {
                    return;
                }
                if let Some(directory) = state.pending.pop() {
                    state.busy += 1;
                    break directory;
                }
                if state.busy == 0 {
                    queue.ready.notify_all();
                    return;
                }
                state = queue.ready.wait(state).expect("walk queue");
            }
        };
        let outcome = visit(&directory);
        let mut state = queue.state.lock().expect("walk queue");
        state.busy -= 1;
        match outcome {
            Ok(children) => state.pending.extend(children),
            Err(error) => {
                state.failure.get_or_insert(error);
            }
        }
        queue.ready.notify_all();
    }
}

/// Visit the entries of `directory`; returns the subdirectories to descend into.
fn visit_directory(root: &Path, directory: &Path, keep: &Keep<'_>) -> Result<Vec<PathBuf>> {
    let mut descend = Vec::new();
    for (name, entry) in read_directory(directory)? {
        let path = directory.join(&name);
        let relative = path.strip_prefix(root).map_err(|_| {
            ForkspaceError::StateFailed(format!(
                "{} escaped the walk of {}",
                path.display(),
                root.display()
            ))
        })?;
        if keep(relative, &path, &entry)? && entry.is_dir() {
            descend.push(path);
        }
    }
    Ok(descend)
}

/// The entries of `directory` with their facts.
fn read_directory(directory: &Path) -> Result<Vec<(PathBuf, Entry)>> {
    if BULK.load(Ordering::Relaxed) != DISTRUSTED {
        if let Some(entries) = bulk_entries(directory) {
            if BULK.load(Ordering::Relaxed) == TRUSTED || verified(directory, &entries) {
                return Ok(entries);
            }
        } else {
            BULK.store(DISTRUSTED, Ordering::Relaxed);
        }
    }
    plain_entries(directory)
}

/// Check bulk results against `lstat` until some non-directory entries have been compared
/// (directories come from `lstat` already). The first difference distrusts bulk reads for
/// the rest of the process.
fn verified(directory: &Path, entries: &[(PathBuf, Entry)]) -> bool {
    let checked: Vec<bool> = entries
        .iter()
        .filter(|(_, entry)| !entry.is_dir())
        .take(64)
        .map(|(name, entry)| Entry::lstat(&directory.join(name)).is_ok_and(|stat| stat == *entry))
        .collect();
    if checked.is_empty() {
        return true;
    }
    let matches = checked.iter().all(|matched| *matched);
    BULK.store(
        if matches { TRUSTED } else { DISTRUSTED },
        Ordering::Relaxed,
    );
    matches
}

/// `readdir` and `lstat`.
fn plain_entries(directory: &Path) -> Result<Vec<(PathBuf, Entry)>> {
    let entries = std::fs::read_dir(directory).map_err(|source| ForkspaceError::Io {
        context: format!("failed to walk {}", directory.display()),
        source,
    })?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ForkspaceError::Io {
            context: format!("failed to walk {}", directory.display()),
            source,
        })?;
        let facts = Entry::lstat(&entry.path())?;
        found.push((PathBuf::from(entry.file_name()), facts));
    }
    Ok(found)
}

/// `getattrlistbulk` over `directory`, or `None` if the call fails.
fn bulk_entries(directory: &Path) -> Option<Vec<(PathBuf, Entry)>> {
    let name = CString::new(directory.as_os_str().as_bytes()).ok()?;
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    let result = read_all(fd, directory);
    unsafe { libc::close(fd) };
    result
}

fn read_all(fd: libc::c_int, directory: &Path) -> Option<Vec<(PathBuf, Entry)>> {
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_CHGTIME
            | libc::ATTR_CMN_ACCESSMASK
            | libc::ATTR_CMN_FILEID
            | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT
            | libc::ATTR_FILE_ALLOCSIZE
            | libc::ATTR_FILE_DATALENGTH,
        forkattr: 0,
    };
    // u64 storage keeps the buffer 8-byte aligned.
    let mut buffer = vec![0_u64; BUFFER_BYTES / 8];
    let mut entries = Vec::new();
    loop {
        let count = unsafe {
            libc::getattrlistbulk(
                fd,
                (&mut request as *mut libc::attrlist).cast(),
                buffer.as_mut_ptr().cast(),
                BUFFER_BYTES,
                0,
            )
        };
        if count < 0 {
            return None;
        }
        if count == 0 {
            return Some(entries);
        }
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), BUFFER_BYTES) };
        let mut offset = 0;
        for _ in 0..count {
            let length = read::<u32>(bytes, offset)? as usize;
            if length == 0 || offset + length > bytes.len() {
                return None;
            }
            let (name, entry) = parse(&bytes[offset..offset + length], directory)?;
            entries.push((name, entry));
            offset += length;
        }
    }
}

/// One entry of a `getattrlistbulk` buffer: `u32` length, the returned attribute set, then
/// each returned attribute in bit order (common, then file attributes).
fn parse(record: &[u8], directory: &Path) -> Option<(PathBuf, Entry)> {
    let mut cursor = 4;
    let returned: libc::attribute_set_t = read(record, cursor)?;
    cursor += std::mem::size_of::<libc::attribute_set_t>();
    let common = returned.commonattr;
    let has = |bit: u32| common & bit != 0;

    let mut name = None;
    if has(libc::ATTR_CMN_NAME) {
        let reference: libc::attrreference_t = read(record, cursor)?;
        let start = cursor.checked_add_signed(reference.attr_dataoffset as isize)?;
        let end = start.checked_add(reference.attr_length as usize)?;
        let text = CStr::from_bytes_until_nul(record.get(start..end)?).ok()?;
        name = Some(PathBuf::from(OsStr::from_bytes(text.to_bytes())));
        cursor += std::mem::size_of::<libc::attrreference_t>();
    }
    let mut dev = 0;
    if has(libc::ATTR_CMN_DEVID) {
        dev = read::<i32>(record, cursor)? as u32 as u64;
        cursor += 4;
    }
    let mut object = 0;
    if has(libc::ATTR_CMN_OBJTYPE) {
        object = read::<u32>(record, cursor)?;
        cursor += 4;
    }
    let mut times = [0_i128; 2];
    for (index, bit) in [libc::ATTR_CMN_MODTIME, libc::ATTR_CMN_CHGTIME]
        .into_iter()
        .enumerate()
    {
        if has(bit) {
            let time: libc::timespec = read(record, cursor)?;
            times[index] = i128::from(time.tv_sec) * 1_000_000_000 + i128::from(time.tv_nsec);
            cursor += std::mem::size_of::<libc::timespec>();
        }
    }
    let mut mode = 0;
    if has(libc::ATTR_CMN_ACCESSMASK) {
        mode = read::<u32>(record, cursor)? & 0o7777;
        cursor += 4;
    }
    let mut ino = 0;
    if has(libc::ATTR_CMN_FILEID) {
        ino = read::<u64>(record, cursor)?;
        cursor += 8;
    }
    if has(ATTR_CMN_ERROR) {
        if read::<u32>(record, cursor)? != 0 {
            return None;
        }
        cursor += 4;
    }
    let name = name?;
    let kind = match object {
        VREG => Kind::File,
        VDIR => Kind::Dir,
        VLNK => Kind::Symlink,
        _ => Kind::Other,
    };
    if kind == Kind::Dir {
        // Directories need their own st_size; take the whole lstat.
        return Some((name.clone(), Entry::lstat(&directory.join(&name)).ok()?));
    }
    let file = returned.fileattr;
    let mut nlink = 1;
    if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        nlink = u64::from(read::<u32>(record, cursor)?);
        cursor += 4;
    }
    let mut allocated = 0;
    if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        allocated = read::<libc::off_t>(record, cursor)? as u64;
        cursor += 8;
    }
    let mut len = 0;
    if file & libc::ATTR_FILE_DATALENGTH != 0 {
        len = read::<libc::off_t>(record, cursor)? as u64;
    }
    Some((
        name,
        Entry {
            kind,
            len,
            mode,
            mtime_ns: times[0],
            ctime_ns: times[1],
            dev,
            ino,
            nlink,
            blocks: allocated / 512,
        },
    ))
}

/// One entry's share of [`private_tree_bytes`].
struct Sized {
    name: PathBuf,
    dir: bool,
    ino: u64,
    nlink: u32,
    /// Private bytes for a file, allocated bytes otherwise; 0 for a directory.
    bytes: u64,
}

/// The entries of `directory` with their private sizes, or `None` if the call fails.
fn private_sizes(directory: &Path) -> Option<Vec<Sized>> {
    let name = CString::new(directory.as_os_str().as_bytes()).ok()?;
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_FILEID
            | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
        forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
    };
    let mut buffer = vec![0_u64; BUFFER_BYTES / 8];
    let mut entries = Vec::new();
    let outcome = loop {
        let count = unsafe {
            libc::getattrlistbulk(
                fd,
                (&mut request as *mut libc::attrlist).cast(),
                buffer.as_mut_ptr().cast(),
                BUFFER_BYTES,
                libc::FSOPT_ATTR_CMN_EXTENDED as u64,
            )
        };
        if count <= 0 {
            break (count == 0).then_some(());
        }
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), BUFFER_BYTES) };
        let mut offset = 0;
        let parsed = (0..count).try_for_each(|_| {
            let length = read::<u32>(bytes, offset)? as usize;
            if length == 0 || offset + length > bytes.len() {
                return None;
            }
            entries.push(parse_sized(&bytes[offset..offset + length])?);
            offset += length;
            Some(())
        });
        if parsed.is_none() {
            break None;
        }
    };
    unsafe { libc::close(fd) };
    outcome.map(|()| entries)
}

/// One record of a [`private_sizes`] buffer: the returned attribute set, then the common,
/// file, and extended common attributes in bit order.
fn parse_sized(record: &[u8]) -> Option<Sized> {
    let mut cursor = 4;
    let returned: libc::attribute_set_t = read(record, cursor)?;
    cursor += std::mem::size_of::<libc::attribute_set_t>();
    let has = |bit: u32| returned.commonattr & bit != 0;
    if !has(libc::ATTR_CMN_NAME) || !has(libc::ATTR_CMN_OBJTYPE) || !has(libc::ATTR_CMN_FILEID) {
        return None;
    }
    let reference: libc::attrreference_t = read(record, cursor)?;
    let start = cursor.checked_add_signed(reference.attr_dataoffset as isize)?;
    let end = start.checked_add(reference.attr_length as usize)?;
    let text = CStr::from_bytes_until_nul(record.get(start..end)?).ok()?;
    let name = PathBuf::from(OsStr::from_bytes(text.to_bytes()));
    cursor += std::mem::size_of::<libc::attrreference_t>();
    let object = read::<u32>(record, cursor)?;
    cursor += 4;
    let ino = read::<u64>(record, cursor)?;
    cursor += 8;
    if has(ATTR_CMN_ERROR) {
        if read::<u32>(record, cursor)? != 0 {
            return None;
        }
        cursor += 4;
    }
    if object == VDIR {
        return Some(Sized {
            name,
            dir: true,
            ino,
            nlink: 1,
            bytes: 0,
        });
    }
    let mut nlink = 1;
    if returned.fileattr & libc::ATTR_FILE_LINKCOUNT != 0 {
        nlink = read::<u32>(record, cursor)?;
        cursor += 4;
    }
    let mut allocated = 0;
    if returned.fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
        allocated = read::<libc::off_t>(record, cursor)? as u64;
        cursor += 8;
    }
    let private = (object == VREG && returned.forkattr & libc::ATTR_CMNEXT_PRIVATESIZE != 0)
        .then(|| read::<libc::off_t>(record, cursor))
        .flatten();
    Some(Sized {
        name,
        dir: false,
        ino,
        nlink,
        bytes: private.map_or(allocated, |bytes| u64::try_from(bytes).unwrap_or(0)),
    })
}

/// Read a `T` at `offset` (attribute buffers are only 4-byte aligned).
fn read<T: Copy>(bytes: &[u8], offset: usize) -> Option<T> {
    let end = offset.checked_add(std::mem::size_of::<T>())?;
    let slice = bytes.get(offset..end)?;
    Some(unsafe { std::ptr::read_unaligned(slice.as_ptr().cast::<T>()) })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::walk::Entry;

    #[test]
    fn bulk_private_sizes_match_per_file_reads() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("dir/deeper")).unwrap();
        std::fs::write(root.path().join("file"), vec![1_u8; 70_000]).unwrap();
        std::fs::hard_link(root.path().join("file"), root.path().join("hard")).unwrap();
        std::fs::write(root.path().join("dir/deeper/inner"), vec![2_u8; 9_000]).unwrap();
        std::os::unix::fs::symlink("file", root.path().join("link")).unwrap();
        // APFS reports unflushed data as private size 0.
        for path in ["file", "dir/deeper/inner"] {
            std::fs::File::open(root.path().join(path))
                .unwrap()
                .sync_all()
                .unwrap();
        }
        let bulk = super::private_tree_bytes(root.path(), 2).expect("bulk private sizes");
        unsafe { std::env::set_var("FORKSPACE_NO_BULK", "1") };
        let per_file = crate::disk::tree_private_bytes(root.path()).unwrap();
        unsafe { std::env::remove_var("FORKSPACE_NO_BULK") };
        assert_eq!(bulk, per_file);
        assert!(bulk >= 79_000, "{bulk}");
    }

    #[test]
    fn bulk_attributes_match_lstat() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("file"), vec![1_u8; 70_000]).unwrap();
        std::fs::hard_link(root.path().join("file"), root.path().join("hard")).unwrap();
        std::os::unix::fs::symlink("file", root.path().join("link")).unwrap();
        std::fs::write(root.path().join("dir/inner"), "x").unwrap();

        let mut entries = super::bulk_entries(root.path()).expect("getattrlistbulk works");
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let names: Vec<PathBuf> = entries.iter().map(|(name, _)| name.clone()).collect();
        assert_eq!(names, ["dir", "file", "hard", "link"].map(PathBuf::from));
        for (name, entry) in entries {
            assert_eq!(
                entry,
                Entry::lstat(&root.path().join(&name)).unwrap(),
                "{name:?}"
            );
        }
    }
}
