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
        // What the project leaves out of workspaces (nested worktrees, `.forkspaceexclude`):
        // the directories above it are made, and everything beside it cloned.
        let excluded = crate::exclude::Exclusions::of(base).present(base);
        if !excluded.is_empty() {
            if fs::symlink_metadata(dest).is_ok() {
                return Err(ForkspaceError::WorkspaceExists(
                    workspace_name(dest).into_owned(),
                ));
            }
            clone_except(base, dest, Path::new(""), &excluded)?;
            if before != sample(base) {
                eprintln!(
                    "warning: base changed while '{}' was being forked; retry the fork for a consistent view",
                    workspace_name(dest)
                );
            }
            return Ok(ForkStats::default());
        }
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
            remedy:
                "omit --tmpfs; for a workspace removed on exit, use `forkspace enter --ephemeral`"
                    .to_string(),
        }
    }
}

/// Clone `relative` of `base` to the same place under `dest`, leaving out the entries
/// `excluded` names (relative to `base`, none of them inside another). A directory with
/// nothing excluded inside is cloned whole; one above an excluded entry is made with the
/// source's permissions and modification time, and its other entries cloned one by one.
fn clone_except(base: &Path, dest: &Path, relative: &Path, excluded: &[PathBuf]) -> Result {
    let source = base.join(relative);
    let target = dest.join(relative);
    let above_excluded = excluded
        .iter()
        .any(|path| path != relative && path.starts_with(relative));
    if !above_excluded {
        return clone_one(&source, &target);
    }
    let metadata =
        fs::symlink_metadata(&source).io_context(|| format!("inspect {}", source.display()))?;
    fs::create_dir(&target).io_context(|| format!("create {}", target.display()))?;
    for entry in fs::read_dir(&source).io_context(|| format!("read {}", source.display()))? {
        let entry = entry.io_context(|| format!("read {}", source.display()))?;
        let child = relative.join(entry.file_name());
        if excluded.contains(&child) {
            continue;
        }
        clone_except(base, dest, &child, excluded)?;
    }
    fs::set_permissions(
        &target,
        std::os::unix::fs::PermissionsExt::from_mode(metadata.mode() & 0o7777),
    )
    .io_context(|| format!("set permissions of {}", target.display()))?;
    if let Ok(modified) = metadata.modified() {
        let _ = fs::File::open(&target)
            .and_then(|dir| dir.set_times(fs::FileTimes::new().set_modified(modified)));
    }
    Ok(())
}

/// `CLONE_NOFOLLOW` from `<sys/clonefile.h>`: clone a symlink itself, not its target.
const CLONE_NOFOLLOW: u32 = 0x0001;

/// One `clonefile(2)` of `source` (a file, symlink or whole directory) to `target`.
fn clone_one(source: &Path, target: &Path) -> Result {
    let source_c = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| ForkspaceError::InvalidArgument("path contains a NUL byte".to_string()))?;
    let target_c = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| ForkspaceError::InvalidArgument("path contains a NUL byte".to_string()))?;
    // SAFETY: both are valid NUL-terminated paths for the duration of the call.
    if unsafe { libc::clonefile(source_c.as_ptr(), target_c.as_ptr(), CLONE_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error())
            .io_context(|| format!("clone {} to {}", source.display(), target.display()));
    }
    Ok(())
}

const SAME_VOLUME_REMEDY: &str =
    "move the project and FORKSPACE_HOME to the same APFS volume for clones instead of a copy";

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
