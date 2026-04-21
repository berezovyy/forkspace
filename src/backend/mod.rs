#[cfg(target_os = "macos")]
mod clone;
mod linux;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::capabilities::Capabilities;
use crate::diff::FileChange;
use crate::error::{ForkspaceError, Result};
use crate::manifest::ManifestStatus;
use crate::workspace::Workspace;

#[cfg(target_os = "macos")]
pub use clone::CloneBackend;
pub use linux::{LinuxBackend, LinuxBackendState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    Linux,
    MacClone,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::MacClone => "mac-clone",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "linux" => Some(Self::Linux),
            "mac-clone" => Some(Self::MacClone),
            _ => None,
        }
    }

    fn platform(self) -> &'static str {
        match self {
            Self::Linux => "Linux",
            Self::MacClone => "macOS",
        }
    }

    pub fn stacking_is_snapshot(self) -> bool {
        matches!(self, Self::MacClone)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum BackendState {
    Linux(LinuxBackendState),
    MacClone {
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
        crate::pull::base_changes(ws)
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
    let requested = match std::env::var("FORKSPACE_BACKEND") {
        Ok(value) => {
            BackendKind::parse(&value).ok_or_else(|| ForkspaceError::BackendUnavailable {
                diagnosis: format!("unknown backend '{value}'"),
                remedy: "use 'linux' or 'mac-clone' for debug overrides".to_string(),
            })?
        }
        Err(_) => recorded.unwrap_or({
            if cfg!(target_os = "macos") {
                BackendKind::MacClone
            } else {
                BackendKind::Linux
            }
        }),
    };

    match requested {
        BackendKind::Linux if cfg!(target_os = "linux") => Ok(Box::new(LinuxBackend::new())),
        #[cfg(target_os = "macos")]
        BackendKind::MacClone => Ok(Box::new(CloneBackend::new())),
        kind => Err(unavailable(kind)),
    }
}

pub(crate) fn pending_create_path(kind: BackendKind, name: &str) -> PathBuf {
    match kind {
        BackendKind::Linux => crate::util::forkspace_home().join(name),
        BackendKind::MacClone => crate::util::forkspace_home().join("workspaces").join(name),
    }
}
