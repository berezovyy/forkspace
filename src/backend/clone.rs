#![cfg(target_os = "macos")]

use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{
    BackendKind, BackendState, ChangeCount, CreateReq, DestroyMode, ViewInfo, WorkspaceBackend,
};
use crate::capabilities::Capabilities;
use crate::diff::FileChange;
use crate::error::{ForkspaceError, Result};
use crate::manifest::{self, ManifestStatus};
use crate::util::{forkspace_home, now_unix_ns};
use crate::workspace::Workspace;

const CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: false,
    instant_fork: false,
    sealed_parent: true,
};

pub struct CloneBackend;

impl CloneBackend {
    pub fn new() -> Self {
        Self
    }

    fn workspace_root() -> PathBuf {
        forkspace_home().join("workspaces")
    }

    fn apfs(path: &Path) -> Result<bool> {
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            ForkspaceError::InvalidArgument("workspace path contains a NUL byte".to_string())
        })?;
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(c_path.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Err(ForkspaceError::Io {
                context: format!("inspect filesystem for {}", path.display()),
                source: std::io::Error::last_os_error(),
            });
        }
        let info = unsafe { info.assume_init() };
        let name = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) };
        Ok(name.to_bytes() == b"apfs")
    }

    fn sample(path: &Path) -> Vec<(PathBuf, u128, u64)> {
        let mut paths = vec![path.to_path_buf()];
        if let Ok(entries) = fs::read_dir(path) {
            paths.extend(
                entries
                    .filter_map(|entry| entry.ok().map(|value| value.path()))
                    .take(16),
            );
        }
        paths.sort();
        paths
            .into_iter()
            .filter_map(|entry| {
                fs::metadata(&entry).ok().map(|metadata| {
                    let nanos = metadata.mtime_nsec().max(0) as u128;
                    (
                        entry,
                        metadata.mtime() as u128 * 1_000_000_000 + nanos,
                        metadata.len(),
                    )
                })
            })
            .collect()
    }
}

impl Default for CloneBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceBackend for CloneBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::MacClone
    }

    fn capabilities(&self) -> &Capabilities {
        &CAPABILITIES
    }

    fn preflight(&self, base: &Path) -> Result {
        if !base.is_dir() {
            return Err(ForkspaceError::InvalidArgument(format!(
                "base path {} is not a directory; choose an existing project directory",
                base.display()
            )));
        }
        let root = Self::workspace_root();
        fs::create_dir_all(&root).map_err(|source| ForkspaceError::Io {
            context: format!("create workspace root {}", root.display()),
            source,
        })?;
        let base = base.canonicalize().map_err(|source| ForkspaceError::Io {
            context: format!("canonicalize base {}", base.display()),
            source,
        })?;
        let root = root.canonicalize().map_err(|source| ForkspaceError::Io {
            context: format!("canonicalize workspace root {}", root.display()),
            source,
        })?;
        if root.starts_with(&base) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "workspace root is inside the project tree".to_string(),
                remedy: "set FORKSPACE_HOME outside the project and retry".to_string(),
            });
        }
        if fs::metadata(&base)
            .map_err(|source| ForkspaceError::Io {
                context: format!("inspect base {}", base.display()),
                source,
            })?
            .dev()
            != fs::metadata(&root)
                .map_err(|source| ForkspaceError::Io {
                    context: format!("inspect workspace root {}", root.display()),
                    source,
                })?
                .dev()
        {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "base and workspace root are on different volumes".to_string(),
                remedy: "set FORKSPACE_HOME to an APFS directory on the base volume".to_string(),
            });
        }
        if !Self::apfs(&base)? {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "base is not on an APFS volume".to_string(),
                remedy: "move the project and FORKSPACE_HOME to the same APFS volume".to_string(),
            });
        }
        Ok(())
    }

    fn create(&self, req: &CreateReq) -> Result<ViewInfo> {
        if req.tmpfs {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "--tmpfs is unavailable with APFS clones".to_string(),
                remedy: "omit --tmpfs and use --ephemeral for automatic cleanup".to_string(),
            });
        }
        let destination = Self::workspace_root().join(&req.name);
        let source = CString::new(req.base.as_os_str().as_bytes()).map_err(|_| {
            ForkspaceError::InvalidArgument("base path contains a NUL byte".to_string())
        })?;
        let target = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
            ForkspaceError::InvalidArgument("workspace path contains a NUL byte".to_string())
        })?;
        let before = Self::sample(&req.base);
        let fork_time_ns = now_unix_ns();
        if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } != 0 {
            let source = std::io::Error::last_os_error();
            let _ = fs::remove_dir_all(&destination);
            return Err(ForkspaceError::Io {
                context: format!(
                    "clone {} to {}; ensure both paths are on the same APFS volume",
                    req.base.display(),
                    destination.display()
                ),
                source,
            });
        }
        if before != Self::sample(&req.base) {
            eprintln!(
                "warning: base changed while '{}' was being forked; retry the fork for a consistent view",
                req.name
            );
        }
        let manifest_status =
            if manifest::spawn(req.name.clone(), destination.clone(), fork_time_ns).is_ok() {
                ManifestStatus::Building
            } else {
                ManifestStatus::Degraded
            };
        Ok(ViewInfo {
            view_path: destination,
            fork_time_ns,
            backend_state: BackendState::MacClone {
                unknown_fields: serde_json::Map::new(),
            },
            manifest_status,
        })
    }

    fn destroy(&self, ws: &Workspace, _mode: DestroyMode) -> Result {
        let trash_root = forkspace_home().join("trash");
        fs::create_dir_all(&trash_root).map_err(|source| ForkspaceError::Io {
            context: format!("create trash directory {}", trash_root.display()),
            source,
        })?;
        let trash = trash_root.join(format!(
            "{}-{}-{}",
            ws.name,
            ws.fork_time_ns,
            std::process::id()
        ));
        fs::rename(&ws.view_path, &trash).map_err(|source| ForkspaceError::Io {
            context: format!(
                "move workspace '{}' to trash; close processes using it and retry",
                ws.name
            ),
            source,
        })?;
        let _ = Command::new("rm")
            .args(["-rf", "--"])
            .arg(&trash)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        Ok(())
    }

    fn view_path(&self, ws: &Workspace) -> PathBuf {
        ws.view_path.clone()
    }

    fn changes(
        &self,
        ws: &Workspace,
        include_ignored: bool,
        need_line_counts: bool,
    ) -> Result<Vec<FileChange>> {
        crate::diff::collect_clone_changes(ws, include_ignored, need_line_counts)
    }

    fn change_count(&self, ws: &Workspace, include_ignored: bool) -> Result<ChangeCount> {
        manifest::change_count(ws, include_ignored)
    }

    fn shell_env(&self, ws: &Workspace) -> Vec<(String, String)> {
        vec![(
            "FORKSPACE_VIEW".to_string(),
            ws.view_path.display().to_string(),
        )]
    }
}
