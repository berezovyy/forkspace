use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Map;
use walkdir::WalkDir;

use super::{
    BackendKind, BackendState, ChangeCount, CreateReq, DestroyMode, ViewInfo, WorkspaceBackend,
};
use crate::capabilities::Capabilities;
use crate::diff::FileChange;
use crate::error::{ForkspaceError, Result};
use crate::manifest::{self, ManifestStatus};
use crate::util::{forkspace_home, now_unix_ns, IoContext, WalkDirExt};
use crate::workspace::Workspace;

const REFLINK_CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: true,
    instant_fork: false,
    sealed_parent: true,
};
const COPY_CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: false,
    instant_fork: false,
    sealed_parent: true,
};

pub struct CowDirBackend {
    kind: BackendKind,
}

impl CowDirBackend {
    pub fn new(kind: BackendKind) -> Self {
        debug_assert!(matches!(
            kind,
            BackendKind::LinuxReflink | BackendKind::PlainCopy
        ));
        Self { kind }
    }

    fn destination(name: &str) -> PathBuf {
        forkspace_home().join("workspaces").join(name)
    }

    fn try_reflink(&self, source: &Path, destination: &Path) -> Result<bool> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self;
            let _ = (source, destination);
            Ok(false)
        }
        #[cfg(target_os = "linux")]
        {
            if self.kind != BackendKind::LinuxReflink {
                return Ok(false);
            }
            let input =
                fs::File::open(source).io_context(|| format!("open {}", source.display()))?;
            let output = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination)
                .io_context(|| format!("create {}", destination.display()))?;
            let cloned = super::probe::clone_file(&input, &output);
            if !cloned {
                drop(output);
                fs::remove_file(destination)
                    .io_context(|| format!("reset {}", destination.display()))?;
            }
            Ok(cloned)
        }
    }

    fn materialize(&self, source: &Path, target: &Path) -> Result<(u64, u64, u64)> {
        let mut hardlinks: HashMap<(u64, u64), PathBuf> = HashMap::new();
        let mut reflinked = 0;
        let mut copied = 0;
        let mut bytes = 0;
        let mut directories = Vec::new();
        fs::create_dir_all(target).io_context(|| format!("create {}", target.display()))?;
        for entry in WalkDir::new(source).min_depth(1).follow_links(false) {
            let entry = entry.walk_context(source)?;
            let relative = entry.path().strip_prefix(source).map_err(|_| {
                ForkspaceError::StateFailed("copy walk escaped its source".to_string())
            })?;
            let destination = target.join(relative);
            let metadata = fs::symlink_metadata(entry.path())
                .io_context(|| format!("inspect {}", entry.path().display()))?;
            if metadata.is_dir() {
                fs::create_dir_all(&destination)
                    .io_context(|| format!("create {}", destination.display()))?;
                directories.push((destination, metadata));
                continue;
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).io_context(|| format!("create {}", parent.display()))?;
            }
            if metadata.file_type().is_symlink() {
                std::os::unix::fs::symlink(
                    fs::read_link(entry.path())
                        .io_context(|| format!("read {}", entry.path().display()))?,
                    &destination,
                )
                .io_context(|| format!("create {}", destination.display()))?;
                continue;
            }
            if !metadata.is_file() {
                return Err(ForkspaceError::CapabilityUnsupported {
                    diagnosis: format!("unsupported file kind at {}", relative.display()),
                    remedy: "remove sockets/devices from the project before forking".to_string(),
                });
            }
            let identity = (metadata.dev(), metadata.ino());
            if metadata.nlink() > 1 {
                if let Some(first) = hardlinks.get(&identity) {
                    fs::hard_link(first, &destination)
                        .io_context(|| format!("hardlink {}", destination.display()))?;
                    continue;
                }
            }
            let used_reflink = self.try_reflink(entry.path(), &destination)?;
            if used_reflink {
                reflinked += 1;
            } else {
                fs::copy(entry.path(), &destination).io_context(|| {
                    format!(
                        "copy {} -> {}",
                        entry.path().display(),
                        destination.display()
                    )
                })?;
                copied += 1;
                bytes += metadata.len();
            }
            fs::set_permissions(&destination, metadata.permissions())
                .io_context(|| format!("set permissions on {}", destination.display()))?;
            let modified = metadata
                .modified()
                .io_context(|| format!("read time from {}", entry.path().display()))?;
            fs::OpenOptions::new()
                .write(true)
                .open(&destination)
                .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
                .io_context(|| format!("set time on {}", destination.display()))?;
            if metadata.nlink() > 1 {
                hardlinks.insert(identity, destination);
            }
        }
        for (directory, metadata) in directories.into_iter().rev() {
            fs::set_permissions(
                &directory,
                fs::Permissions::from_mode(metadata.mode() & 0o7777),
            )
            .io_context(|| format!("set permissions on {}", directory.display()))?;
            let modified = metadata
                .modified()
                .io_context(|| format!("read directory time {}", directory.display()))?;
            fs::File::open(&directory)
                .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
                .io_context(|| format!("set time on {}", directory.display()))?;
        }
        Ok((reflinked, copied, bytes))
    }
}

impl WorkspaceBackend for CowDirBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn capabilities(&self) -> &Capabilities {
        match self.kind {
            BackendKind::LinuxReflink => &REFLINK_CAPABILITIES,
            _ => &COPY_CAPABILITIES,
        }
    }

    fn preflight(&self, base: &Path) -> Result {
        if !base.is_dir() {
            return Err(ForkspaceError::InvalidArgument(format!(
                "base path {} is not a directory",
                base.display()
            )));
        }
        let root = forkspace_home().join("workspaces");
        fs::create_dir_all(&root).io_context(|| format!("create {}", root.display()))?;
        let base = base
            .canonicalize()
            .io_context(|| format!("canonicalize {}", base.display()))?;
        let root = root
            .canonicalize()
            .io_context(|| format!("canonicalize {}", root.display()))?;
        if root.starts_with(&base) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "workspace root is inside the project tree".to_string(),
                remedy: "set FORKSPACE_HOME outside the project and retry".to_string(),
            });
        }
        Ok(())
    }

    fn create(&self, req: &CreateReq) -> Result<ViewInfo> {
        if req.tmpfs {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "--tmpfs is unavailable for directory-copy backends".to_string(),
                remedy: "omit --tmpfs or install fuse-overlayfs".to_string(),
            });
        }
        let destination = Self::destination(&req.name);
        if destination.exists() {
            return Err(ForkspaceError::WorkspaceExists(req.name.clone()));
        }
        let started = Instant::now();
        let (reflinked, copied, bytes) = match self.materialize(&req.base, &destination) {
            Ok(stats) => stats,
            Err(error) => {
                let _ = fs::remove_dir_all(&destination);
                return Err(error);
            }
        };
        let mut max_ctime = 0_u128;
        for entry in WalkDir::new(&destination).follow_links(false) {
            let entry = entry.walk_context(&destination)?;
            let metadata = fs::symlink_metadata(entry.path())
                .io_context(|| format!("inspect {}", entry.path().display()))?;
            let ctime = (metadata.ctime().max(0) as u128) * 1_000_000_000
                + metadata.ctime_nsec().max(0) as u128;
            max_ctime = max_ctime.max(ctime);
        }
        let fork_time_ns = now_unix_ns().max(max_ctime).saturating_add(1);
        manifest::build(&req.name, &destination, fork_time_ns)?;
        let elapsed_ms = started.elapsed().as_millis();
        let (degraded, reason) = if self.kind == BackendKind::PlainCopy {
            (
                true,
                Some(format!(
                    "used a full copy ({bytes} bytes in {elapsed_ms} ms); install fuse-overlayfs or use a reflink-capable volume for faster forks"
                )),
            )
        } else {
            (
                false,
                Some(format!(
                    "used reflinks for {reflinked} files ({copied} files required copying)"
                )),
            )
        };
        Ok(ViewInfo {
            view_path: destination,
            fork_time_ns,
            backend_state: BackendState::CowDir {
                reflinked_files: reflinked,
                copied_files: copied,
                unknown_fields: Map::new(),
            },
            manifest_status: ManifestStatus::Ready,
            degraded,
            probe_reason: reason,
        })
    }

    fn destroy(&self, ws: &Workspace, _mode: DestroyMode) -> Result {
        let trash_root = forkspace_home().join("trash");
        fs::create_dir_all(&trash_root)
            .io_context(|| format!("create {}", trash_root.display()))?;
        let trash = trash_root.join(format!("{}-{}", ws.name, now_unix_ns()));
        fs::rename(&ws.view_path, &trash)
            .io_context(|| format!("move workspace '{}' to trash", ws.name))?;
        fs::remove_dir_all(&trash).io_context(|| format!("remove {}", trash.display()))
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
