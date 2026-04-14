use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::backend::{BackendKind, BackendState, LinuxBackendState, ViewInfo};
use crate::error::{ForkspaceError, Result};
use crate::util::{forkspace_home, now_rfc3339, now_unix_ns};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningProcess {
    pub pid: u32,
    pub command: String,
    pub port: u16,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub base: PathBuf,
    pub parent: Option<String>,
    pub created: String,
    pub session: Option<String>,
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<RunningProcess>,
    pub view_path: PathBuf,
    pub fork_time_ns: u128,
    pub backend_kind: BackendKind,
    pub backend_state: BackendState,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

impl Workspace {
    pub fn new(name: &str, base: PathBuf, parent: Option<String>) -> Self {
        let root = forkspace_home().join(name);
        let merged = root.join("merged");
        Self {
            name: name.to_string(),
            base,
            parent,
            created: now_rfc3339(),
            session: None,
            ephemeral: false,
            process: None,
            view_path: merged.clone(),
            fork_time_ns: now_unix_ns(),
            backend_kind: BackendKind::Linux,
            backend_state: BackendState::Linux(LinuxBackendState {
                upper: root.join("upper"),
                work: root.join("work"),
                merged,
                tmpfs: false,
                tmpfs_size: None,
                unknown_fields: Map::new(),
            }),
            unknown_fields: Map::new(),
        }
    }

    pub fn from_view(
        name: String,
        base: PathBuf,
        parent: Option<String>,
        session: Option<String>,
        backend_kind: BackendKind,
        view: ViewInfo,
    ) -> Self {
        Self {
            name,
            base,
            parent,
            created: now_rfc3339(),
            session,
            ephemeral: false,
            process: None,
            view_path: view.view_path,
            fork_time_ns: view.fork_time_ns,
            backend_kind,
            backend_state: view.backend_state,
            unknown_fields: Map::new(),
        }
    }

    pub fn linux_state(&self) -> Result<&LinuxBackendState> {
        match &self.backend_state {
            BackendState::Linux(state) => Ok(state),
            BackendState::MacClone { .. } => Err(ForkspaceError::BackendUnavailable {
                diagnosis: "linux backend state is unavailable for this macOS workspace"
                    .to_string(),
                remedy: "open this workspace with forkspace on macOS".to_string(),
            }),
        }
    }

    pub fn upper_path(&self) -> Result<&Path> {
        Ok(&self.linux_state()?.upper)
    }

    pub fn work_path(&self) -> Result<&Path> {
        Ok(&self.linux_state()?.work)
    }

    pub fn tmpfs(&self) -> bool {
        self.linux_state().is_ok_and(|state| state.tmpfs)
    }

    pub fn tmpfs_size(&self) -> Option<&str> {
        self.linux_state()
            .ok()
            .and_then(|state| state.tmpfs_size.as_deref())
    }
}
