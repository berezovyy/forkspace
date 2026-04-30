//! macOS fork primitive: one recursive APFS `clonefile(2)` of the whole base directory.
//! The clone itself is near-instant, so the fork manifest is built in the background.

#![cfg(target_os = "macos")]

use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::Map;

use super::{ForkPrimitive, ForkStats, ForkSummary};
use crate::backend::{BackendKind, BackendState, Capabilities};
use crate::error::{ForkspaceError, Result};
use crate::util::IoContext;

const CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: false,
    instant_fork: false,
    sealed_parent: false,
};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ClonefilePrimitive;

impl ForkPrimitive for ClonefilePrimitive {
    fn kind(&self) -> BackendKind {
        BackendKind::MacClone
    }

    fn capabilities(&self) -> &'static Capabilities {
        &CAPABILITIES
    }

    fn check_available(&self, base: &Path, dest_parent: &Path) -> Result {
        let unavailable = |diagnosis: String| ForkspaceError::BackendUnavailable {
            diagnosis,
            remedy: SAME_VOLUME_REMEDY.to_string(),
        };
        let (Ok(base_meta), Ok(destination_meta)) = (fs::metadata(base), fs::metadata(dest_parent))
        else {
            return Err(unavailable(
                "base or workspace root cannot be inspected".to_string(),
            ));
        };
        if base_meta.dev() != destination_meta.dev() {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "base and workspace root are on different volumes".to_string(),
                remedy: SAME_VOLUME_REMEDY.to_string(),
            });
        }
        match filesystem_type(base) {
            Ok(kind) if kind == "apfs" => Ok(()),
            Ok(kind) => Err(unavailable(format!(
                "{} is {kind}; APFS clones are unavailable there",
                base.display()
            ))),
            Err(error) => Err(unavailable(error.to_string())),
        }
    }

    fn materialize(&self, base: &Path, dest: &Path) -> Result<ForkStats> {
        let source = CString::new(base.as_os_str().as_bytes()).map_err(|_| {
            ForkspaceError::InvalidArgument("base path contains a NUL byte".to_string())
        })?;
        let target = CString::new(dest.as_os_str().as_bytes()).map_err(|_| {
            ForkspaceError::InvalidArgument("workspace path contains a NUL byte".to_string())
        })?;
        let before = sample(base);
        if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                return Err(ForkspaceError::WorkspaceExists(
                    workspace_name(dest).into_owned(),
                ));
            }
            return Err(error).io_context(|| {
                format!(
                    "clone {} to {}; ensure both paths are on the same APFS volume",
                    base.display(),
                    dest.display()
                )
            });
        }
        // clonefile is not atomic with respect to concurrent writers in the base.
        if before != sample(base) {
            eprintln!(
                "warning: base changed while '{}' was being forked; retry the fork for a consistent view",
                workspace_name(dest)
            );
        }
        Ok(ForkStats::default())
    }

    fn fork_time_ns(&self, started_ns: u128, _stats: &ForkStats) -> u128 {
        started_ns
    }

    fn summarize(&self, _stats: &ForkStats, _elapsed_ms: u128) -> ForkSummary {
        ForkSummary::default()
    }

    fn manifest_in_background(&self) -> bool {
        true
    }

    fn backend_state(&self, _stats: &ForkStats) -> BackendState {
        BackendState::MacClone {
            unknown_fields: Map::new(),
        }
    }

    fn tmpfs_unsupported(&self) -> ForkspaceError {
        ForkspaceError::CapabilityUnsupported {
            diagnosis: "--tmpfs is unavailable with APFS clones".to_string(),
            remedy: "omit --tmpfs and use --ephemeral for automatic cleanup".to_string(),
        }
    }
}

const SAME_VOLUME_REMEDY: &str =
    "move the project and FORKSPACE_HOME to the same APFS volume, or pass --copy to accept a full copy";

fn workspace_name(dest: &Path) -> std::borrow::Cow<'_, str> {
    dest.file_name()
        .map_or_else(|| "workspace".into(), |name| name.to_string_lossy())
}

/// The filesystem type name (e.g. `apfs`) of the volume holding `path`.
fn filesystem_type(path: &Path) -> Result<String> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ForkspaceError::InvalidArgument("path contains a NUL byte".to_string()))?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .io_context(|| format!("inspect filesystem for {}", path.to_string_lossy()));
    }
    let info = unsafe { info.assume_init() };
    Ok(unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned())
}

/// A cheap fingerprint of `path` and up to 16 of its entries, used to detect a base that
/// changed while it was being cloned.
fn sample(path: &Path) -> Vec<(PathBuf, i64, i64, u64)> {
    let mut paths = vec![path.to_path_buf()];
    if let Ok(entries) = fs::read_dir(path) {
        paths.extend(entries.flatten().map(|entry| entry.path()).take(16));
    }
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let metadata = fs::metadata(&path).ok()?;
            Some((
                path,
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.len(),
            ))
        })
        .collect()
}
