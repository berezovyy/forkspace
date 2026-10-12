//! How much disk forkspace uses, measured with each platform's own mechanism, and keeping
//! its internal stores out of backups.
//!
//! "Private" bytes are the bytes that belong only to one entry: blocks no other file shares.
//! A cloned file costs nothing until it is changed, so counting allocated blocks would count
//! every clone as a full copy:
//!
//! - Linux Btrfs and XFS: the file's extents that `FIEMAP` does not mark shared.
//! - macOS APFS: `ATTR_CMNEXT_PRIVATESIZE` from `getattrlist`.
//! - Everything else (ext4, tmpfs, …): allocated blocks, since nothing is shared there.
//!
//! These are "private now" numbers: a shared block becomes private when its other owner goes.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use crate::backend::BackendState;
use crate::error::Result;
use crate::paths;
use crate::workspace::Workspace;

/// The private bytes of the tree at `root` (0 if it does not exist). Hard links count once.
pub fn tree_private_bytes(root: &Path) -> Result<u64> {
    if fs::symlink_metadata(root).is_err() {
        return Ok(0);
    }
    let shared_aware = os::shares_blocks(root);
    let seen = Mutex::new(HashSet::new());
    let sizes = crate::util::walk::parallel(root, |_, path, entry| {
        if entry.nlink > 1
            && !seen
                .lock()
                .expect("hard link set")
                .insert((entry.dev, entry.ino))
        {
            return Ok(None);
        }
        let allocated = entry.blocks * 512;
        let bytes = if shared_aware && entry.is_file() {
            os::private_bytes(path).unwrap_or(allocated)
        } else {
            allocated
        };
        Ok(Some(bytes))
    })?;
    Ok(sizes.into_iter().map(|(_, bytes)| bytes).sum())
}

/// The private bytes of a workspace: its upper layer for an overlay, its view otherwise.
pub fn workspace_bytes(workspace: &Workspace) -> Result<u64> {
    match &workspace.backend_state {
        BackendState::Overlay(layers) => tree_private_bytes(&layers.upper),
        BackendState::MacClone { .. } | BackendState::CowDir { .. } => {
            tree_private_bytes(&workspace.view_path)
        }
    }
}

/// Bytes in forkspace's stores other than the workspaces themselves.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct StoreUsage {
    /// Frozen copies of projects (sealed overlays).
    pub frozen: u64,
    /// Undo data and checkpoints.
    pub history: u64,
    /// Fork-time copies, trash, and journals.
    pub other: u64,
}

impl StoreUsage {
    pub fn measure() -> Result<Self> {
        Ok(Self {
            frozen: frozen_bytes()?,
            history: tree_private_bytes(&paths::undo_root())?
                + tree_private_bytes(&paths::checkpoint_root())?,
            other: tree_private_bytes(&paths::ancestor_root())?
                + tree_private_bytes(&paths::trash_dir())?
                + tree_private_bytes(&paths::journal_dir())?,
        })
    }
}

/// The private bytes of all frozen copies together (hard links between them count once).
/// Frozen copies never change, so the total is cached in `frozen/.usage` and measured again
/// only when the set of copies changes.
fn frozen_bytes() -> Result<u64> {
    let root = paths::frozen_root();
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(0);
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    ids.sort();
    let key = ids.join(",");
    let cache = root.join(".usage");
    if let Ok(text) = fs::read_to_string(&cache) {
        if let Some((cached_key, bytes)) = text.trim_end().rsplit_once(' ') {
            if cached_key == key {
                if let Ok(bytes) = bytes.parse() {
                    return Ok(bytes);
                }
            }
        }
    }
    let bytes = tree_private_bytes(&root)?;
    let _ = fs::write(&cache, format!("{key} {bytes}\n"));
    Ok(bytes)
}

/// `1.2 GB`, `340 MB`, `12 KB`, decimal units like most file managers.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Mark `directory` as a cache that backups skip: a `CACHEDIR.TAG` (honoured by
/// `tar --exclude-caches`, borg, and restic) and, on macOS, a sticky Time Machine exclusion.
pub fn exclude_from_backups(directory: &Path) {
    let tag = directory.join("CACHEDIR.TAG");
    if tag.exists() {
        return;
    }
    let _ = fs::write(
        &tag,
        "Signature: 8a477f597d28d172789f06886806bc55\n\
         # This directory holds forkspace's internal data (frozen copies, undo data,\n\
         # checkpoints, or trash). It can be rebuilt or is disposable; backups skip it.\n\
         # For information about cache directory tags see https://bford.info/cachedir/\n",
    );
    os::exclude_from_time_machine(directory);
}

#[cfg(target_os = "linux")]
mod os {
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    /// `FS_IOC_FIEMAP`.
    const FIEMAP: libc::c_ulong = 0xC020_660B;
    const EXTENT_LAST: u32 = 0x1;
    const EXTENT_SHARED: u32 = 0x2000;
    const EXTENTS_PER_CALL: usize = 64;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }

    #[repr(C)]
    struct Request {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
        extents: [Extent; EXTENTS_PER_CALL],
    }

    /// Btrfs and XFS can share blocks between files (reflinks).
    pub fn shares_blocks(path: &Path) -> bool {
        let Ok(name) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
            return false;
        };
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return false;
        }
        let kind = unsafe { stats.assume_init() }.f_type as u64;
        kind == 0x9123_683E || kind == 0x5846_5342
    }

    /// Bytes in extents of the file at `path` that no other file shares.
    pub fn private_bytes(path: &Path) -> Option<u64> {
        let file = fs::File::open(path).ok()?;
        let mut private = 0_u64;
        let mut start = 0_u64;
        loop {
            let mut request = Request {
                start,
                length: u64::MAX - start,
                flags: 0,
                mapped_extents: 0,
                extent_count: EXTENTS_PER_CALL as u32,
                reserved: 0,
                extents: [Extent::default(); EXTENTS_PER_CALL],
            };
            // SAFETY: `request` is a valid fiemap buffer for `EXTENTS_PER_CALL` extents.
            if unsafe { libc::ioctl(file.as_raw_fd(), FIEMAP as _, &mut request) } != 0 {
                return None;
            }
            let mapped = request.mapped_extents as usize;
            if mapped == 0 {
                return Some(private);
            }
            for extent in &request.extents[..mapped.min(EXTENTS_PER_CALL)] {
                if extent.flags & EXTENT_SHARED == 0 {
                    private += extent.length;
                }
                if extent.flags & EXTENT_LAST != 0 {
                    return Some(private);
                }
            }
            let last = request.extents[mapped.min(EXTENTS_PER_CALL) - 1];
            start = last.logical + last.length;
        }
    }

    pub fn exclude_from_time_machine(_directory: &Path) {}
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::CString;
    use std::path::Path;

    /// APFS shares blocks between clones.
    pub fn shares_blocks(path: &Path) -> bool {
        let Ok(name) = CString::new(path.as_os_str().as_encoded_bytes()) else {
            return false;
        };
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return false;
        }
        let stats = unsafe { stats.assume_init() };
        let kind: Vec<u8> = stats
            .f_fstypename
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        kind == b"apfs"
    }

    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        returned: libc::attribute_set_t,
        private_size: libc::off_t,
    }

    /// `ATTR_CMNEXT_PRIVATESIZE`: the bytes of `path` no clone shares.
    pub fn private_bytes(path: &Path) -> Option<u64> {
        let name = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        let mut request = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
        };
        let mut reply = std::mem::MaybeUninit::<Reply>::zeroed();
        let status = unsafe {
            libc::getattrlist(
                name.as_ptr(),
                (&mut request as *mut libc::attrlist).cast(),
                reply.as_mut_ptr().cast(),
                std::mem::size_of::<Reply>(),
                libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW,
            )
        };
        if status != 0 {
            return None;
        }
        let reply = unsafe { reply.assume_init() };
        let returned = reply.returned.forkattr;
        (returned & libc::ATTR_CMNEXT_PRIVATESIZE != 0)
            .then(|| u64::try_from(reply.private_size).unwrap_or(0))
    }

    /// `tmutil addexclusion` (without `-p`) sets a sticky exclusion that needs no root.
    pub fn exclude_from_time_machine(directory: &Path) {
        let _ = std::process::Command::new("/usr/bin/tmutil")
            .arg("addexclusion")
            .arg(directory)
            .output();
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod os {
    use std::path::Path;

    pub fn shares_blocks(_path: &Path) -> bool {
        false
    }
    pub fn private_bytes(_path: &Path) -> Option<u64> {
        None
    }
    pub fn exclude_from_time_machine(_directory: &Path) {}
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    /// A clone of a file costs (almost) nothing until it is changed.
    #[test]
    fn apfs_clones_have_no_private_size() {
        let root = tempfile::tempdir().unwrap();
        if !super::os::shares_blocks(root.path()) {
            eprintln!("skipping: the temporary directory is not on APFS");
            return;
        }
        let original = root.path().join("original");
        std::fs::write(&original, vec![3_u8; 4 << 20]).unwrap();
        let clones = root.path().join("clones");
        std::fs::create_dir(&clones).unwrap();
        let cloned = std::process::Command::new("cp")
            .arg("-c")
            .arg(&original)
            .arg(clones.join("copy"))
            .status()
            .unwrap();
        assert!(cloned.success());
        let private = super::tree_private_bytes(&clones).unwrap();
        assert!(
            private < 64 * 1024,
            "clone counted as {private} private bytes"
        );
        assert!(super::tree_private_bytes(root.path()).unwrap() >= 4 << 20);
    }
}
