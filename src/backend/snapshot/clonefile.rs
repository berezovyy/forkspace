#![cfg(target_os = "macos")]

use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::{Degradation, ForkPrimitive, ForkStats, ForkTimePolicy, ProbeResult};
use crate::backend::BackendKind;
use crate::error::{ForkspaceError, Result};

#[derive(Debug, Clone, Copy, Default)]
pub struct ClonefilePrimitive;

impl ClonefilePrimitive {
    fn filesystem_type(path: &Path) -> Result<String> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| ForkspaceError::InvalidArgument("path contains a NUL byte".to_string()))?;
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Err(ForkspaceError::Io {
                context: format!("inspect filesystem for {}", path.to_string_lossy()),
                source: std::io::Error::last_os_error(),
            });
        }
        let info = unsafe { info.assume_init() };
        Ok(unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned())
    }

    fn sample(path: &Path) -> Vec<(PathBuf, i64, i64, u64)> {
        let mut paths = vec![path.to_path_buf()];
        if let Ok(entries) = fs::read_dir(path) {
            paths.extend(entries.flatten().map(|entry| entry.path()).take(16));
        }
        paths.sort();
        paths
            .into_iter()
            .filter_map(|path| {
                fs::metadata(&path).ok().map(|metadata| {
                    (
                        path,
                        metadata.mtime(),
                        metadata.mtime_nsec(),
                        metadata.len(),
                    )
                })
            })
            .collect()
    }
}

impl ForkPrimitive for ClonefilePrimitive {
    fn kind(&self) -> BackendKind {
        BackendKind::MacClone
    }

    fn probe(&self, base: &Path, dest_parent: &Path) -> ProbeResult {
        let (Ok(base_meta), Ok(destination_meta)) = (fs::metadata(base), fs::metadata(dest_parent))
        else {
            return ProbeResult::unavailable("base or workspace root cannot be inspected");
        };
        if base_meta.dev() != destination_meta.dev() {
            return ProbeResult::unavailable("base and workspace root are on different volumes");
        }
        match Self::filesystem_type(base) {
            Ok(kind) if kind == "apfs" => {
                ProbeResult::available("recursive APFS clonefile is available")
            }
            Ok(kind) => ProbeResult::unavailable(format!(
                "{} is {kind}; APFS clones are unavailable there",
                base.display()
            )),
            Err(error) => ProbeResult::unavailable(error.to_string()),
        }
    }

    fn materialize(&self, base: &Path, dest: &Path) -> Result<ForkStats> {
        let source = CString::new(base.as_os_str().as_bytes()).map_err(|_| {
            ForkspaceError::InvalidArgument("base path contains a NUL byte".to_string())
        })?;
        let target = CString::new(dest.as_os_str().as_bytes()).map_err(|_| {
            ForkspaceError::InvalidArgument("workspace path contains a NUL byte".to_string())
        })?;
        let before = Self::sample(base);
        if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } != 0 {
            let source = std::io::Error::last_os_error();
            if source.raw_os_error() == Some(libc::EEXIST) {
                return Err(ForkspaceError::WorkspaceExists(dest.file_name().map_or_else(
                    || "workspace".into(),
                    |name| name.to_string_lossy().into(),
                )));
            }
            return Err(ForkspaceError::Io {
                context: format!(
                    "clone {} to {}; ensure both paths are on the same APFS volume",
                    base.display(),
                    dest.display()
                ),
                source,
            });
        }
        if before != Self::sample(base) {
            eprintln!(
                "warning: base changed while '{}' was being forked; retry the fork for a consistent view",
                dest.file_name().map_or_else(|| "workspace".into(), |name| name.to_string_lossy())
            );
        }
        Ok(ForkStats::default())
    }

    fn fork_time_policy(&self) -> ForkTimePolicy {
        ForkTimePolicy::Before
    }

    fn degradation(&self) -> Option<Degradation> {
        None
    }
}
