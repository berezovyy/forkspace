//! RAM-backed overlay layers for `--tmpfs` workspaces.
//!
//! Mounting a tmpfs needs privileges forkspace does not have, so the writable layers go on a
//! tmpfs that already exists: the per-user runtime directory (`$XDG_RUNTIME_DIR`), or a
//! private directory under `/dev/shm`. Only `upper/` and `work/` live there; the merged view
//! stays under `FORKSPACE_HOME`. The layers vanish on reboot, and the tmpfs's own size limit
//! (set by the system, not by forkspace) caps how much a workspace can write.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{ForkspaceError, Result};
use crate::util::{ensure_private_dir, IoContext};

/// The `upper/` and `work/` directories of one tmpfs-backed workspace.
pub(crate) struct TmpfsLayers {
    pub upper: PathBuf,
    pub work: PathBuf,
}

/// Create fresh `upper/` and `work/` directories for workspace `name` on a tmpfs.
pub(crate) fn create_layers(name: &str, fork_time_ns: u128) -> Result<TmpfsLayers> {
    let root = layer_root()?.join(format!("{name}-{fork_time_ns}"));
    let layers = TmpfsLayers {
        upper: root.join("upper"),
        work: root.join("work"),
    };
    for path in [&layers.upper, &layers.work] {
        fs::create_dir_all(path).io_context(|| format!("create tmpfs layer {}", path.display()))?;
    }
    Ok(layers)
}

/// Remove the tmpfs directory holding `upper`. A directory already gone (after a reboot)
/// is not an error.
pub(crate) fn remove_layers(upper: &Path) -> Result {
    let Some(root) = upper.parent() else {
        return Ok(());
    };
    match fs::remove_dir_all(root) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).io_context(|| format!("remove tmpfs layers {}", root.display()))
        }
        _ => Ok(()),
    }
}

/// The private directory that holds every tmpfs workspace's layers.
fn layer_root() -> Result<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if is_tmpfs(&runtime) {
            let root = runtime.join("forkspace");
            ensure_private_dir(&root)?;
            return Ok(root);
        }
    }
    let shm = Path::new("/dev/shm");
    if is_tmpfs(shm) {
        let root = shm.join(format!("forkspace-{}", unsafe { libc::geteuid() }));
        ensure_private_dir(&root)?;
        ensure_owned_directory(&root)?;
        return Ok(root);
    }
    Err(ForkspaceError::CapabilityUnsupported {
        diagnosis:
            "--tmpfs needs an existing tmpfs, and neither $XDG_RUNTIME_DIR nor /dev/shm is one"
                .to_string(),
        remedy: "omit --tmpfs to use disk-backed workspace storage".to_string(),
    })
}

/// `/dev/shm` is shared by every user, so the directory found there must be a real directory
/// owned by us, not a symlink or another user's directory planted under our name.
fn ensure_owned_directory(path: &Path) -> Result {
    use std::os::unix::fs::MetadataExt;

    let metadata =
        fs::symlink_metadata(path).io_context(|| format!("inspect {}", path.display()))?;
    if metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() } {
        return Ok(());
    }
    Err(ForkspaceError::CapabilityUnsupported {
        diagnosis: format!("{} is not a directory owned by you", path.display()),
        remedy: format!(
            "remove {} or set XDG_RUNTIME_DIR to your runtime directory",
            path.display()
        ),
    })
}

#[cfg(target_os = "linux")]
fn is_tmpfs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    const TMPFS_MAGIC: i64 = 0x0102_1994;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` is a valid out-pointer for one statfs.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: statfs returned 0, so it initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    #[allow(clippy::unnecessary_cast)]
    let filesystem = stat.f_type as i64;
    filesystem == TMPFS_MAGIC
}

#[cfg(not(target_os = "linux"))]
fn is_tmpfs(_path: &Path) -> bool {
    false
}
