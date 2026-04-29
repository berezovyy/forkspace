//! Workspace backends: how a workspace view is materialized, scanned, and destroyed.
//!
//! Every command dispatches through [`WorkspaceBackend`]. The backend kind is chosen once at
//! fork time (by capability probe) and persisted, so later commands use [`select_backend`]
//! with the recorded kind.

pub mod overlay;
pub mod probe;
pub mod snapshot;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::changes::manifest::ManifestStatus;
use crate::changes::FileChange;
use crate::error::{ForkspaceError, Result};
use crate::workspace::Workspace;

pub use overlay::{OverlayBackend, OverlayState};
pub use snapshot::{
    Degradation, ForkPrimitive, ForkStats, ForkTimePolicy, ProbeResult, SnapshotBackend,
};

/// Static feature flags a backend advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub tmpfs_backing: bool,
    pub network_namespace: bool,
    pub instant_fork: bool,
    pub sealed_parent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    #[serde(rename = "linux", alias = "linux-overlay")]
    Overlay,
    MacClone,
    LinuxReflink,
    #[serde(rename = "copy")]
    PlainCopy,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overlay => "linux",
            Self::MacClone => "mac-clone",
            Self::LinuxReflink => "linux-reflink",
            Self::PlainCopy => "copy",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "linux" | "linux-overlay" => Some(Self::Overlay),
            "mac-clone" => Some(Self::MacClone),
            "linux-reflink" => Some(Self::LinuxReflink),
            "copy" => Some(Self::PlainCopy),
            _ => None,
        }
    }

    fn platform(self) -> &'static str {
        match self {
            Self::Overlay => "Linux",
            Self::MacClone => "macOS",
            Self::LinuxReflink => "Linux",
            Self::PlainCopy => "any supported platform",
        }
    }

    pub fn stacking_is_snapshot(self) -> bool {
        matches!(self, Self::MacClone | Self::LinuxReflink | Self::PlainCopy)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum BackendState {
    #[serde(rename = "linux")]
    Overlay(OverlayState),
    MacClone {
        #[serde(flatten)]
        unknown_fields: Map<String, Value>,
    },
    CowDir {
        #[serde(default)]
        reflinked_files: u64,
        #[serde(default)]
        copied_files: u64,
        #[serde(flatten)]
        unknown_fields: Map<String, Value>,
    },
}

pub struct CreateReq {
    pub name: String,
    pub base: PathBuf,
    pub tmpfs: bool,
    pub tmpfs_size: Option<String>,
}

pub struct ViewInfo {
    pub view_path: PathBuf,
    pub fork_time_ns: u128,
    pub backend_state: BackendState,
    pub manifest_status: ManifestStatus,
    pub degraded: bool,
    pub probe_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeCount {
    Ready(usize),
    Pending,
    Degraded,
}

#[derive(Debug, Clone, Copy)]
pub enum DestroyMode {
    Normal,
}

pub trait WorkspaceBackend {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> &Capabilities;
    fn preflight(&self, base: &Path) -> Result<()>;
    fn create(&self, req: &CreateReq) -> Result<ViewInfo>;
    fn destroy(&self, ws: &Workspace, mode: DestroyMode) -> Result<()>;
    fn view_path(&self, ws: &Workspace) -> PathBuf;
    fn changes(
        &self,
        ws: &Workspace,
        include_ignored: bool,
        need_line_counts: bool,
    ) -> Result<Vec<FileChange>>;
    fn change_count(&self, ws: &Workspace, include_ignored: bool) -> Result<ChangeCount>;
    fn base_changes(&self, ws: &Workspace) -> Result<Vec<FileChange>> {
        crate::sync::pull::base_changes(ws)
    }
    fn shell_env(&self, ws: &Workspace) -> Vec<(String, String)>;
}

fn unavailable(kind: BackendKind) -> ForkspaceError {
    ForkspaceError::BackendUnavailable {
        diagnosis: format!(
            "{} workspace backend is unavailable in this build",
            kind.as_str()
        ),
        remedy: format!("open this workspace with forkspace on {}", kind.platform()),
    }
}

pub fn select_backend(recorded: Option<BackendKind>) -> Result<Box<dyn WorkspaceBackend>> {
    let requested = match recorded {
        Some(kind) => kind,
        None => match std::env::var("FORKSPACE_BACKEND") {
            Ok(value) => {
                BackendKind::parse(&value).ok_or_else(|| ForkspaceError::BackendUnavailable {
                    diagnosis: format!("unknown backend '{value}'"),
                    remedy:
                        "use 'linux', 'mac-clone', 'linux-reflink', or 'copy' for debug overrides"
                            .to_string(),
                })?
            }
            Err(_) => {
                if cfg!(target_os = "macos") {
                    BackendKind::MacClone
                } else {
                    BackendKind::Overlay
                }
            }
        },
    };

    match requested {
        BackendKind::Overlay if cfg!(target_os = "linux") => Ok(Box::new(OverlayBackend::new())),
        #[cfg(target_os = "macos")]
        BackendKind::MacClone => Ok(Box::new(SnapshotBackend::new(snapshot::ClonefilePrimitive))),
        BackendKind::LinuxReflink if cfg!(target_os = "linux") => {
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(SnapshotBackend::new(snapshot::ReflinkPrimitive)))
            }
            #[cfg(not(target_os = "linux"))]
            {
                unreachable!()
            }
        }
        BackendKind::PlainCopy => Ok(Box::new(SnapshotBackend::new(snapshot::CopyPrimitive))),
        kind => Err(unavailable(kind)),
    }
}

pub fn select_backend_for_create(
    base: &Path,
    force_copy: bool,
) -> Result<Box<dyn WorkspaceBackend>> {
    if force_copy {
        return Ok(Box::new(SnapshotBackend::new(snapshot::CopyPrimitive)));
    }
    if std::env::var_os("FORKSPACE_BACKEND").is_some() {
        return select_backend(None);
    }
    #[cfg(target_os = "linux")]
    {
        let report = probe::linux_report(base, &crate::paths::snapshot_root());
        match report.winner() {
            Some(BackendKind::Overlay) => Ok(Box::new(OverlayBackend::new())),
            Some(BackendKind::LinuxReflink) => {
                Ok(Box::new(SnapshotBackend::new(snapshot::ReflinkPrimitive)))
            }
            Some(BackendKind::PlainCopy) => {
                Ok(Box::new(SnapshotBackend::new(snapshot::CopyPrimitive)))
            }
            _ => Err(ForkspaceError::BackendUnavailable {
                diagnosis: "no workspace materialization primitive passed its capability probe"
                    .to_string(),
                remedy: "ensure FORKSPACE_HOME is writable and retry".to_string(),
            }),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = base;
        Ok(Box::new(SnapshotBackend::new(snapshot::ClonefilePrimitive)))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = base;
        Ok(Box::new(SnapshotBackend::new(snapshot::CopyPrimitive)))
    }
}

pub(crate) fn pending_create_path(kind: BackendKind, name: &str) -> PathBuf {
    match kind {
        BackendKind::Overlay => crate::paths::overlay_root(name),
        BackendKind::MacClone => crate::paths::snapshot_dir(name),
        BackendKind::LinuxReflink | BackendKind::PlainCopy => crate::paths::snapshot_dir(name),
    }
}
