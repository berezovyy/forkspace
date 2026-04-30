//! One-way migration of version-1 state files (overlay-only, flat paths) to version 2.
//!
//! The migration is crash-safe: a `.migrating` marker plus a fsynced `state.v1.bak` backup
//! are written first, and an interrupted migration is rolled back on the next load.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use super::{ProxyConfig, State, CURRENT_VERSION};
use crate::backend::{BackendKind, BackendState, OverlayState};
use crate::changes::manifest::ManifestStatus;
use crate::error::{ForkspaceError, Result};
use crate::util::fs::{sync_dir, sync_file, write_atomic};
use crate::util::IoContext;
use crate::workspace::{RunningProcess, Workspace};

#[derive(Deserialize)]
struct LegacyState {
    #[serde(default, rename = "version")]
    _version: u32,
    #[serde(default)]
    workspaces: HashMap<String, LegacyWorkspace>,
    #[serde(default)]
    proxy: Option<ProxyConfig>,
    #[serde(flatten)]
    unknown_fields: Map<String, Value>,
}

#[derive(Deserialize)]
struct LegacyWorkspace {
    name: String,
    base: PathBuf,
    upper: PathBuf,
    work: PathBuf,
    merged: PathBuf,
    parent: Option<String>,
    created: String,
    session: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    tmpfs: bool,
    #[serde(default)]
    tmpfs_size: Option<String>,
    #[serde(default)]
    ephemeral: bool,
    #[serde(default)]
    process: Option<RunningProcess>,
    #[serde(flatten)]
    unknown_fields: Map<String, Value>,
}

impl From<LegacyWorkspace> for Workspace {
    fn from(value: LegacyWorkspace) -> Self {
        Self {
            name: value.name,
            base: value.base,
            parent: value.parent,
            created: value.created,
            session: value.session,
            branch: value.branch,
            ephemeral: value.ephemeral,
            process: value.process,
            view_path: value.merged.clone(),
            fork_time_ns: 0,
            backend_kind: BackendKind::Overlay,
            backend_state: BackendState::Overlay(OverlayState {
                upper: value.upper,
                work: value.work,
                merged: value.merged,
                tmpfs: value.tmpfs,
                tmpfs_size: value.tmpfs_size,
                unknown_fields: Map::new(),
            }),
            manifest_status: ManifestStatus::Absent,
            manifest_generation: 0,
            last_pull: None,
            degraded: false,
            probe_reason: None,
            unknown_fields: value.unknown_fields,
        }
    }
}

/// The v1 backup and the in-progress marker, both next to the state file.
fn migration_paths(path: &Path) -> (PathBuf, PathBuf) {
    let parent = parent_dir(path);
    (parent.join("state.v1.bak"), parent.join(".migrating"))
}

fn parent_dir(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

/// Roll back a migration that was interrupted before its marker was removed.
pub(super) fn recover_interrupted(path: &Path) -> Result {
    let (backup, marker) = migration_paths(path);
    if !marker.exists() {
        return Ok(());
    }
    if backup.exists() {
        fs::copy(&backup, path).io_context(|| {
            format!(
                "restore interrupted state migration from {}",
                backup.display()
            )
        })?;
        sync_file(path)?;
    }
    fs::remove_file(&marker)
        .io_context(|| format!("remove migration marker {}", marker.display()))?;
    sync_dir(parent_dir(path))
}

/// Parse a version-1 state file and persist it as the current version.
pub(super) fn migrate_v1(path: &Path, content: &str, value: Value) -> Result<State> {
    let legacy: LegacyState = serde_json::from_value(value).map_err(|e| {
        ForkspaceError::StateCorrupted(format!("invalid version 1 state file: {e}"))
    })?;
    let (backup, marker) = migration_paths(path);
    fs::write(&marker, b"1 -> 2\n")
        .io_context(|| format!("create migration marker {}", marker.display()))?;
    sync_file(&marker)?;
    write_atomic(&backup, content.as_bytes())?;

    let state = State {
        version: CURRENT_VERSION,
        workspaces: legacy
            .workspaces
            .into_iter()
            .map(|(name, workspace)| (name, workspace.into()))
            .collect(),
        proxy: legacy.proxy,
        unknown_fields: legacy.unknown_fields,
        ..State::default()
    };
    state.save()?;
    fs::remove_file(&marker)
        .io_context(|| format!("remove migration marker {}", marker.display()))?;
    sync_dir(parent_dir(path))?;
    Ok(state)
}
