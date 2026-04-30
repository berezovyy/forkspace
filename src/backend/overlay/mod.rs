//! Linux overlay backend: a fuse-overlayfs lower/upper/work/merged view.
//!
//! The upper directory holds exactly the workspace's writes, so it doubles as the change set.

pub mod mount;
mod whiteout;

pub(crate) use whiteout::{is_device_whiteout, Whiteout};

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    BackendKind, BackendState, Capabilities, ChangeCount, CreateReq, DestroyMode, ViewInfo,
    WorkspaceBackend,
};
use crate::changes::filter::ChangeFilter;
use crate::changes::manifest::ManifestStatus;
use crate::changes::{self, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::util::IoContext;
use crate::workspace::Workspace;
use mount::{
    mount_overlay, tmpfs_not_implemented, unmount_overlay, unmount_tmpfs, OverlayOpts,
    DEFAULT_TMPFS_SIZE,
};

const CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: true,
    instant_fork: true,
    sealed_parent: true,
};

/// Overlay layer directories, persisted in `state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayState {
    pub upper: PathBuf,
    pub work: PathBuf,
    pub merged: PathBuf,
    #[serde(default)]
    pub tmpfs: bool,
    #[serde(default)]
    pub tmpfs_size: Option<String>,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

pub struct OverlayBackend;

impl OverlayBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for OverlayBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Removes the overlay-incompatible files a project lists in `.forkspaceclean` (one view-relative
/// path per line; `#` comments) from a freshly mounted view.
fn apply_forkspaceclean(view: &Path) {
    let Ok(file) = fs::File::open(view.join(".forkspaceclean")) else {
        return;
    };
    let mut cleaned = 0usize;
    for line in std::io::BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
    {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let target = view.join(line);
        if target.exists() && fs::remove_file(target).is_ok() {
            cleaned += 1;
        }
    }
    if cleaned > 0 {
        eprintln!("forkspaceclean: removed {cleaned} overlay-incompatible file(s)");
    }
}

impl WorkspaceBackend for OverlayBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Overlay
    }

    fn capabilities(&self) -> &Capabilities {
        &CAPABILITIES
    }

    fn preflight(&self, base: &Path) -> Result<()> {
        if !base.exists() {
            return Err(ForkspaceError::Io {
                context: format!("base path does not exist: {}", base.display()),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "base path not found"),
            });
        }
        if !base.is_dir() {
            return Err(ForkspaceError::Io {
                context: format!("base path is not a directory: {}", base.display()),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a directory"),
            });
        }
        Ok(())
    }

    fn create(&self, req: &CreateReq) -> Result<ViewInfo> {
        if req.tmpfs && !self.capabilities().tmpfs_backing {
            return Err(tmpfs_not_implemented());
        }

        let root = paths::overlay_root(&req.name);
        let upper = root.join("upper");
        let work = root.join("work");
        let merged = root.join("merged");
        for path in [&upper, &work, &merged] {
            fs::create_dir_all(path)
                .io_context(|| format!("failed to create workspace dir: {}", path.display()))?;
        }

        let opts = OverlayOpts {
            tmpfs: req.tmpfs,
            tmpfs_size: req
                .tmpfs_size
                .clone()
                .unwrap_or_else(|| DEFAULT_TMPFS_SIZE.to_string()),
        };
        if let Err(error) = mount_overlay(&req.base, &upper, &work, &merged, &opts) {
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }
        apply_forkspaceclean(&merged);
        let fork_time_ns = crate::util::now_unix_ns();
        if let Err(error) = crate::changes::manifest::build(&req.name, &req.base, fork_time_ns) {
            let _ = unmount_overlay(&merged);
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }

        Ok(ViewInfo {
            view_path: merged.clone(),
            fork_time_ns,
            backend_state: BackendState::Overlay(OverlayState {
                upper,
                work,
                merged,
                tmpfs: req.tmpfs,
                tmpfs_size: req.tmpfs_size.clone(),
                unknown_fields: Map::new(),
            }),
            manifest_status: ManifestStatus::Ready,
            degraded: false,
            probe_reason: None,
        })
    }

    fn destroy(&self, ws: &Workspace, _mode: DestroyMode) -> Result<()> {
        let state = ws.linux_state()?;
        unmount_overlay(&state.merged)?;
        if state.tmpfs {
            unmount_tmpfs(&state.upper)?;
            unmount_tmpfs(&state.work)?;
        }
        let root = paths::overlay_root(&ws.name);
        fs::remove_dir_all(&root).io_context(|| {
            format!(
                "remove workspace directory {}; retry `forkspace drop {}` after closing processes using it",
                root.display(),
                ws.name
            )
        })
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
        let changes = if need_line_counts {
            changes::collect_changes(ws)?
        } else {
            changes::collect_changes_fast(ws)?
        };
        if include_ignored {
            Ok(changes)
        } else {
            Ok(ChangeFilter::build(&ws.base).filter_changes(changes))
        }
    }

    fn change_count(&self, ws: &Workspace, include_ignored: bool) -> Result<ChangeCount> {
        Ok(ChangeCount::Ready(
            self.changes(ws, include_ignored, false)?.len(),
        ))
    }

    fn shell_env(&self, ws: &Workspace) -> Vec<(String, String)> {
        let mut env = vec![("FORKSPACE_VIEW".to_string(), ws.view_path.display().to_string())];
        if let Ok(state) = ws.linux_state() {
            env.push(("FORKSPACE_UPPER".to_string(), state.upper.display().to_string()));
        }
        env
    }
}
