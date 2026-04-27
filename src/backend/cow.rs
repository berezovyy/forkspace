use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Map;

use super::{
    BackendKind, BackendState, ChangeCount, CreateReq, DestroyMode, ViewInfo, WorkspaceBackend,
};
use crate::capabilities::Capabilities;
use crate::diff::FileChange;
use crate::error::{ForkspaceError, Result};
use crate::manifest::{self, ManifestStatus};
use crate::util::{forkspace_home, now_unix_ns, IoContext};
use crate::workspace::Workspace;

const REFLINK_CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: true,
    instant_fork: false,
    sealed_parent: false,
};
const COPY_CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: true,
    instant_fork: false,
    sealed_parent: false,
};
const CLONE_CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: false,
    instant_fork: false,
    sealed_parent: false,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkTimePolicy {
    Before,
    AfterMaterialize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degradation {
    pub diagnosis: String,
    pub remedy: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub available: bool,
    pub reason: String,
}

impl ProbeResult {
    pub fn available(reason: impl Into<String>) -> Self {
        Self {
            available: true,
            reason: reason.into(),
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkStats {
    pub reflinked_files: u64,
    pub copied_files: u64,
    pub copied_bytes: u64,
    pub max_ctime_ns: u128,
}

impl ForkStats {
    pub(crate) fn observe(&mut self, metadata: &fs::Metadata) {
        let ctime = (metadata.ctime().max(0) as u128) * 1_000_000_000
            + metadata.ctime_nsec().max(0) as u128;
        self.max_ctime_ns = self.max_ctime_ns.max(ctime);
    }
}

pub trait ForkPrimitive: Send + Sync + 'static {
    fn kind(&self) -> BackendKind;
    fn probe(&self, base: &Path, dest_parent: &Path) -> ProbeResult;
    fn materialize(&self, base: &Path, dest: &Path) -> Result<ForkStats>;
    fn fork_time_policy(&self) -> ForkTimePolicy;
    fn degradation(&self) -> Option<Degradation>;
}

pub struct CowDirBackend<P: ForkPrimitive> {
    primitive: P,
}

impl<P: ForkPrimitive> CowDirBackend<P> {
    pub fn new(primitive: P) -> Self {
        Self { primitive }
    }

    fn workspace_root() -> PathBuf {
        forkspace_home().join("workspaces")
    }

    fn destination(name: &str) -> PathBuf {
        Self::workspace_root().join(name)
    }
}

impl<P: ForkPrimitive> WorkspaceBackend for CowDirBackend<P> {
    fn kind(&self) -> BackendKind {
        self.primitive.kind()
    }

    fn capabilities(&self) -> &Capabilities {
        match self.kind() {
            BackendKind::MacClone => &CLONE_CAPABILITIES,
            BackendKind::LinuxReflink => &REFLINK_CAPABILITIES,
            BackendKind::PlainCopy => &COPY_CAPABILITIES,
            BackendKind::Linux => unreachable!("overlay is not a directory primitive"),
        }
    }

    fn preflight(&self, base: &Path) -> Result {
        if !base.is_dir() {
            return Err(ForkspaceError::InvalidArgument(format!(
                "base path {} is not a directory",
                base.display()
            )));
        }
        let root = Self::workspace_root();
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
        let probe = self.primitive.probe(&base, &root);
        if !probe.available {
            let remedy = self.primitive.degradation().map_or_else(
                || {
                    "move the project and FORKSPACE_HOME to the same APFS volume, or pass --copy to accept a full copy"
                        .to_string()
                },
                |value| value.remedy,
            );
            if self.kind() == BackendKind::MacClone && probe.reason.contains("different volumes") {
                return Err(ForkspaceError::CapabilityUnsupported {
                    diagnosis: probe.reason,
                    remedy,
                });
            }
            return Err(ForkspaceError::BackendUnavailable {
                diagnosis: probe.reason,
                remedy,
            });
        }
        Ok(())
    }

    fn create(&self, req: &CreateReq) -> Result<ViewInfo> {
        if req.tmpfs {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: if self.kind() == BackendKind::MacClone {
                    "--tmpfs is unavailable with APFS clones".to_string()
                } else {
                    "--tmpfs is unavailable for directory-copy backends".to_string()
                },
                remedy: if self.kind() == BackendKind::MacClone {
                    "omit --tmpfs and use --ephemeral for automatic cleanup".to_string()
                } else {
                    "omit --tmpfs or install fuse-overlayfs".to_string()
                },
            });
        }
        let destination = Self::destination(&req.name);
        if destination.exists() {
            return Err(ForkspaceError::WorkspaceExists(req.name.clone()));
        }
        let started = Instant::now();
        let before = now_unix_ns();
        let stats = match self.primitive.materialize(&req.base, &destination) {
            Ok(stats) => stats,
            Err(error) => {
                let _ = fs::remove_dir_all(&destination);
                return Err(error);
            }
        };
        let fork_time_ns = match self.primitive.fork_time_policy() {
            ForkTimePolicy::Before => before,
            ForkTimePolicy::AfterMaterialize => {
                now_unix_ns().max(stats.max_ctime_ns).saturating_add(1)
            }
        };
        let mut manifest_failure = None;
        let manifest_status = if self.kind() == BackendKind::MacClone {
            match manifest::spawn(req.name.clone(), destination.clone(), fork_time_ns) {
                Ok(()) => ManifestStatus::Building,
                Err(error) => {
                    manifest_failure = Some(error.to_string());
                    ManifestStatus::Degraded
                }
            }
        } else {
            match manifest::build(&req.name, &destination, fork_time_ns) {
                Ok(_) => ManifestStatus::Ready,
                Err(error) => {
                    manifest_failure = Some(error.to_string());
                    ManifestStatus::Degraded
                }
            }
        };
        let elapsed_ms = started.elapsed().as_millis();
        let (mut degraded, mut probe_reason) = match self.kind() {
            BackendKind::PlainCopy => (
                true,
                Some(format!(
                    "used a full copy ({} bytes in {elapsed_ms} ms); install fuse-overlayfs or use a reflink-capable volume for faster forks",
                    stats.copied_bytes
                )),
            ),
            BackendKind::LinuxReflink => (
                false,
                Some(format!(
                    "used reflinks for {} files ({} files required copying)",
                    stats.reflinked_files, stats.copied_files
                )),
            ),
            BackendKind::MacClone => (false, None),
            BackendKind::Linux => unreachable!("overlay is not a directory primitive"),
        };
        if let Some(error) = manifest_failure {
            degraded = true;
            let tracking = format!(
                "change tracking is degraded because the fork manifest could not be written: {error}"
            );
            probe_reason = Some(match probe_reason {
                Some(reason) => format!("{reason}; {tracking}"),
                None => tracking,
            });
        }
        let backend_state = if self.kind() == BackendKind::MacClone {
            BackendState::MacClone {
                unknown_fields: Map::new(),
            }
        } else {
            BackendState::CowDir {
                reflinked_files: stats.reflinked_files,
                copied_files: stats.copied_files,
                unknown_fields: Map::new(),
            }
        };
        Ok(ViewInfo {
            view_path: destination,
            fork_time_ns,
            backend_state,
            manifest_status,
            degraded,
            probe_reason,
        })
    }

    fn destroy(&self, ws: &Workspace, _mode: DestroyMode) -> Result {
        let trash_root = forkspace_home().join("trash");
        fs::create_dir_all(&trash_root)
            .io_context(|| format!("create {}", trash_root.display()))?;
        let trash = trash_root.join(format!(
            "{}-{}-{}",
            ws.name,
            ws.fork_time_ns,
            std::process::id()
        ));
        fs::rename(&ws.view_path, &trash)
            .io_context(|| format!("move workspace '{}' to trash", ws.name))?;
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
