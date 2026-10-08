//! The workspace record persisted in `state.json`, and its lifecycle.

mod create;
mod env;
pub mod guard;
mod private_git;
pub mod remove;

pub use create::{create, CreateOptions};
pub use env::{inherited_venvs, process_env, InheritedVenv};
pub use remove::remove;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::backend::{BackendKind, BackendState, OverlayState, ViewInfo};
use crate::changes::manifest::ManifestStatus;
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::process::watch;
use crate::util::{now_rfc3339, now_unix_ns};

/// The dev server `forkspace run` started in a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningProcess {
    /// Leader of the process group.
    pub pid: u32,
    /// Guards against PID reuse; see [`crate::process::watch`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    pub command: String,
    /// The app's `$PORT`, which the proxy forwards to.
    pub port: u16,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

impl RunningProcess {
    /// Whether this exact process (not a PID recycler) is still running.
    pub fn is_alive(&self) -> bool {
        watch::is_alive(self.pid, self.identity.as_deref())
    }

    /// Terminate the process group; returns whether it was still running.
    pub fn terminate(&self) -> bool {
        watch::terminate_group(self.pid, self.identity.as_deref())
    }
}

/// One workspace as recorded in `state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    /// The canonical directory this workspace was forked from (a parent's view if stacked).
    pub base: PathBuf,
    /// The workspace this one is stacked on, if any.
    pub parent: Option<String>,
    /// RFC 3339 creation time.
    pub created: String,
    pub session: Option<String>,
    /// Git branch created by `new --branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Removed automatically when the `enter` session that created it exits.
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<RunningProcess>,
    /// The directory users work in.
    pub view_path: PathBuf,
    /// Fork time; also part of the trashed view's name, which crash recovery relies on.
    pub fork_time_ns: u128,
    pub backend_kind: BackendKind,
    pub backend_state: BackendState,
    #[serde(default, skip_serializing_if = "manifest_absent")]
    pub manifest_status: ManifestStatus,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub manifest_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pull: Option<String>,
    /// The backend fell back to a slower path (e.g. a full copy) or lost change tracking.
    #[serde(default, skip_serializing_if = "is_false")]
    pub degraded: bool,
    /// `HEAD` at fork time, for workspaces given a private repository because the base's
    /// `.git` pointed outside it (a linked worktree or submodule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_head_at_fork: Option<String>,
    /// Why the backend behaved as it did, shown to the user after `new`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_reason: Option<String>,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

impl Workspace {
    /// A fresh overlay record laid out under [`paths::overlay_root`], with nothing created
    /// on disk. Used to build records directly, e.g. in tests.
    pub fn new(name: &str, base: PathBuf, parent: Option<String>) -> Self {
        let layers = OverlayState::at(&paths::overlay_root(name));
        let view = ViewInfo {
            view_path: layers.merged.clone(),
            fork_time_ns: now_unix_ns(),
            backend_state: BackendState::Overlay(layers),
            manifest_status: ManifestStatus::Absent,
            degraded: false,
            probe_reason: None,
        };
        Self {
            manifest_generation: 0,
            ..Self::from_view(
                name.to_string(),
                base,
                parent,
                None,
                BackendKind::Overlay,
                view,
            )
        }
    }

    /// The record for a view a backend just materialized.
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
            git_head_at_fork: None,
            degraded: view.degraded,
            probe_reason: view.probe_reason,
            unknown_fields: Map::new(),
        }
    }

    /// Workspaces can be compared side by side only if they share this key: the same
    /// canonical base and the same parent.
    pub fn compare_key(&self) -> (PathBuf, Option<String>) {
        (
            self.base
                .canonicalize()
                .unwrap_or_else(|_| self.base.clone()),
            self.parent.clone(),
        )
    }

    /// The overlay layers, or [`ForkspaceError::BackendUnavailable`] for a snapshot workspace.
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

    /// The overlay's writable layer, where this workspace's changes live.
    pub fn upper_path(&self) -> Result<&Path> {
        Ok(&self.linux_state()?.upper)
    }

    /// The overlay's scratch directory.
    pub fn work_path(&self) -> Result<&Path> {
        Ok(&self.linux_state()?.work)
    }

    /// Whether this is an overlay over the live base (`new --live`, or recorded before frozen
    /// copies existed), so later base changes show through.
    pub fn is_live(&self) -> bool {
        self.linux_state().is_ok_and(|state| state.lower.is_none())
    }

    /// The tree this workspace's changes are measured against: a sealed overlay's frozen copy
    /// (the newest one after a `pull`), otherwise the base.
    pub fn ancestor_dir(&self) -> &Path {
        match &self.backend_state {
            BackendState::Overlay(state) => state
                .ancestor
                .as_deref()
                .or(state.lower.as_deref())
                .unwrap_or(&self.base),
            BackendState::MacClone { .. } | BackendState::CowDir { .. } => &self.base,
        }
    }

    /// Whether the overlay layers live on tmpfs (always false for snapshot workspaces).
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
