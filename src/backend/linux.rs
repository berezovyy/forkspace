use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    BackendKind, BackendState, ChangeCount, CreateReq, DestroyMode, ViewInfo, WorkspaceBackend,
};
use crate::capabilities::Capabilities;
use crate::diff::{self, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::filter::ChangeFilter;
use crate::manifest::ManifestStatus;
use crate::overlay::{mount_overlay, unmount_overlay, unmount_tmpfs, OverlayOpts};
use crate::util::{forkspace_home, IoContext};
use crate::workspace::Workspace;

const CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: true,
    instant_fork: true,
    sealed_parent: true,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxBackendState {
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

pub struct LinuxBackend;

impl LinuxBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LinuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

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

impl WorkspaceBackend for LinuxBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Linux
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
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "--tmpfs is not implemented".to_string(),
                remedy: "omit --tmpfs to use disk-backed workspace storage".to_string(),
            });
        }

        let root = forkspace_home().join(&req.name);
        let upper = root.join("upper");
        let work = root.join("work");
        let merged = root.join("merged");
        for path in [&upper, &work, &merged] {
            fs::create_dir_all(path)
                .io_context(|| format!("failed to create workspace dir: {}", path.display()))?;
        }

        let opts = OverlayOpts {
            tmpfs: req.tmpfs,
            tmpfs_size: req.tmpfs_size.clone().unwrap_or_else(|| "256m".to_string()),
        };
        if let Err(error) = mount_overlay(&req.base, &upper, &work, &merged, &opts) {
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }
        apply_forkspaceclean(&merged);

        Ok(ViewInfo {
            view_path: merged.clone(),
            fork_time_ns: crate::util::now_unix_ns(),
            backend_state: BackendState::Linux(LinuxBackendState {
                upper,
                work,
                merged,
                tmpfs: req.tmpfs,
                tmpfs_size: req.tmpfs_size.clone(),
                unknown_fields: Map::new(),
            }),
            manifest_status: ManifestStatus::Absent,
        })
    }

    fn destroy(&self, ws: &Workspace, _mode: DestroyMode) -> Result<()> {
        let state = ws.linux_state()?;
        unmount_overlay(&state.merged)?;
        if state.tmpfs {
            unmount_tmpfs(&state.upper)?;
            unmount_tmpfs(&state.work)?;
        }
        fs::remove_dir_all(forkspace_home().join(&ws.name)).map_err(|source| ForkspaceError::Io {
            context: format!(
                "remove workspace directory {}; retry `forkspace drop {}` after closing processes using it",
                forkspace_home().join(&ws.name).display(),
                ws.name
            ),
            source,
        })
    }

    fn view_path(&self, ws: &Workspace) -> PathBuf {
        ws.view_path.clone()
    }

    fn changes(&self, ws: &Workspace) -> Result<Vec<FileChange>> {
        diff::collect_changes(ws)
    }

    fn change_count(&self, ws: &Workspace, include_ignored: bool) -> Result<ChangeCount> {
        let changes = self.changes(ws)?;
        let count = if include_ignored {
            changes.len()
        } else {
            ChangeFilter::build(&ws.base).filter_changes(changes).len()
        };
        Ok(ChangeCount::Ready(count))
    }

    fn shell_env(&self, ws: &Workspace) -> Vec<(String, String)> {
        let mut env = vec![(
            "FORKSPACE_VIEW".to_string(),
            ws.view_path.display().to_string(),
        )];
        if let Ok(state) = ws.linux_state() {
            env.push((
                "FORKSPACE_UPPER".to_string(),
                state.upper.display().to_string(),
            ));
        }
        env
    }
}
