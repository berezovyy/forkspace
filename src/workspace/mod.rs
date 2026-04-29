//! The workspace record persisted in `state.json`, and its lifecycle.

mod create;
pub mod guard;
pub mod remove;

pub use create::{create, CreateOptions};
pub use remove::remove;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::backend::{BackendKind, BackendState, OverlayState, ViewInfo};
use crate::changes::manifest::ManifestStatus;
use crate::error::{ForkspaceError, Result};
use crate::util::{now_rfc3339, now_unix_ns};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningProcess {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<RunningProcess>,
    pub view_path: PathBuf,
    pub fork_time_ns: u128,
    pub backend_kind: BackendKind,
    pub backend_state: BackendState,
    #[serde(default, skip_serializing_if = "manifest_absent")]
    pub manifest_status: ManifestStatus,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub manifest_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pull: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub degraded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_reason: Option<String>,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

impl Workspace {
    pub fn compare_key(&self) -> (PathBuf, Option<String>) {
        (
            self.base
                .canonicalize()
                .unwrap_or_else(|_| self.base.clone()),
            self.parent.clone(),
        )
    }

    pub fn new(name: &str, base: PathBuf, parent: Option<String>) -> Self {
        let root = crate::paths::overlay_root(name);
        let merged = root.join("merged");
        Self {
            name: name.to_string(),
            base,
            parent,
            created: now_rfc3339(),
            session: None,
            branch: None,
            ephemeral: false,
            process: None,
            view_path: merged.clone(),
            fork_time_ns: now_unix_ns(),
            backend_kind: BackendKind::Overlay,
            backend_state: BackendState::Overlay(OverlayState {
                upper: root.join("upper"),
                work: root.join("work"),
                merged,
                tmpfs: false,
                tmpfs_size: None,
                unknown_fields: Map::new(),
            }),
            manifest_status: ManifestStatus::Absent,
            manifest_generation: 0,
            last_pull: None,
            degraded: false,
            probe_reason: None,
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
            branch: None,
            ephemeral: false,
            process: None,
            view_path: view.view_path,
            fork_time_ns: view.fork_time_ns,
            backend_kind,
            backend_state: view.backend_state,
            manifest_status: view.manifest_status,
            manifest_generation: 1,
            last_pull: None,
            degraded: view.degraded,
            probe_reason: view.probe_reason,
            unknown_fields: Map::new(),
        }
    }

    pub fn linux_state(&self) -> Result<&OverlayState> {
        match &self.backend_state {
            BackendState::Overlay(state) => Ok(state),
            BackendState::MacClone { .. } | BackendState::CowDir { .. } => {
                Err(ForkspaceError::BackendUnavailable {
                    diagnosis: "linux backend state is unavailable for this macOS workspace"
                        .to_string(),
                    remedy: "open this workspace with forkspace on macOS".to_string(),
                })
            }
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

fn manifest_absent(status: &ManifestStatus) -> bool {
    *status == ManifestStatus::Absent
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}
