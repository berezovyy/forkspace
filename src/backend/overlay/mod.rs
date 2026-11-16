//! Linux overlay backend: a fuse-overlayfs lower/upper/work/merged view.
//!
//! The lower layer is a frozen copy of the base ([`frozen`]), so later changes to the base
//! never show through; `--live` workspaces use the base itself instead. The upper directory
//! holds exactly the workspace's writes, so it doubles as the change set.

pub(crate) mod frozen;
pub mod mount;
mod tmpfs;
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
use crate::error::Result;
use crate::paths;
use crate::util::{trace, IoContext};
use crate::workspace::Workspace;
pub use frozen::flush_pending;
use mount::{mount_overlay, unmount_overlay, ViewHealth};

const CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: true,
    network_namespace: true,
    instant_fork: true,
    sealed_parent: true,
};

/// Overlay layer directories, persisted in `state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayState {
    /// The frozen copy mounted as the lower layer; `None` for a live workspace, whose lower
    /// layer is the base itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower: Option<PathBuf>,
    /// The frozen copy changes are measured against, when it is newer than `lower`: `pull`
    /// re-baselines on a fresh copy of the base without remounting the view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<PathBuf>,
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

impl OverlayState {
    /// The standard `upper/`, `work/`, `merged/` layout under `root`.
    pub fn at(root: &Path) -> Self {
        Self {
            lower: None,
            ancestor: None,
            upper: root.join("upper"),
            work: root.join("work"),
            merged: root.join("merged"),
            tmpfs: false,
            tmpfs_size: None,
            unknown_fields: Map::new(),
        }
    }
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

/// fuse-overlayfs reads any `.wh.<name>` entry in the lower layer as a whiteout, so a base
/// file with that name would vanish from the view together with `<name>`. Refuse such bases
/// rather than fork a view that silently hides files.
/// Record workspace `name`'s fork manifest of its lower layer `lower`. A frozen copy whose
/// manifest an earlier fork recorded (and checked) gets a copy of it; otherwise the layer is
/// walked and checked, and a clean result is kept for the next fork of the same copy.
fn record_manifest(
    name: &str,
    frozen: Option<&frozen::Frozen>,
    lower: &Path,
    fork_time_ns: u128,
) -> Result {
    use crate::changes::manifest;

    if let Some(frozen) = frozen {
        let cache = frozen::manifest_cache(&frozen.tree);
        if cache.exists() && manifest::install_copy(name, &cache).is_ok() {
            return Ok(());
        }
    }
    manifest::build(name, lower, fork_time_ns)?;
    let clean = refuse_reserved_names(name)?;
    if let Some(frozen) = frozen.filter(|_| clean) {
        // Only an optimization for the next fork: a failure here costs that fork a walk.
        let _ = frozen::save_manifest_cache(&frozen.tree, &manifest::manifest_path(name));
    }
    Ok(())
}

/// Refuse a lower layer with a `.wh.` path component, which fuse-overlayfs reserves for
/// deletion markers. Returns whether no row is dirty: an entry written while the manifest was
/// recorded makes the manifest specific to this fork, so it must not be reused.
fn refuse_reserved_names(name: &str) -> Result<bool> {
    let mut clean = true;
    let manifest = crate::changes::manifest::Manifest::open(name)?;
    for row in manifest.iter() {
        clean &= !row.dirty;
        if frozen::has_whiteout_name(row.path) {
            let path = crate::changes::manifest::decode_path(row.path)?;
            return Err(crate::error::ForkspaceError::CapabilityUnsupported {
                diagnosis: format!(
                    "{} uses the .wh. prefix, which fuse-overlayfs reserves for deletion markers",
                    path.display()
                ),
                remedy: "rename that path, or fork with --copy (or FORKSPACE_BACKEND=linux-reflink on a reflink filesystem)".to_string(),
            });
        }
    }
    Ok(clean)
}

/// The frozen copy a new workspace mounts: of the project, or, when stacking on a sealed
/// workspace, of that workspace's view with its unchanged files linked from its own copy.
fn freeze_base(req: &CreateReq) -> Result<frozen::Frozen> {
    let basis = match req.parent_layers.as_ref() {
        Some(OverlayState {
            lower: Some(lower),
            upper,
            ..
        }) => frozen::Basis::Workspace { lower, upper },
        _ => frozen::Basis::Project,
    };
    frozen::freeze(&req.base, basis)
}

/// What a new frozen copy cost, shown after `new` (nothing when an existing copy was reused).
fn describe_freeze(frozen: &frozen::Frozen) -> Option<String> {
    if !frozen.created || frozen.copied_files == 0 {
        return None;
    }
    Some(format!(
        "froze a copy of the base for this workspace (copied {} files, {:.1} MiB; \
         linked {} unchanged files); later forks reuse it",
        frozen.copied_files,
        frozen.copied_bytes as f64 / f64::from(1 << 20),
        frozen.linked_files
    ))
}

impl WorkspaceBackend for OverlayBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Overlay
    }

    fn capabilities(&self) -> &Capabilities {
        &CAPABILITIES
    }

    fn preflight(&self, base: &Path) -> Result<()> {
        super::require_directory(base)
    }

    fn create(&self, req: &CreateReq) -> Result<ViewInfo> {
        let root = paths::overlay_root(&req.name);
        let frozen = if req.live {
            None
        } else {
            Some(trace::time("freeze", || freeze_base(req))?)
        };
        let fork_time_ns = crate::util::now_unix_ns();
        let mut layers = OverlayState::at(&root);
        layers.lower = frozen.as_ref().map(|frozen| frozen.tree.clone());
        let lower = layers.lower.clone().unwrap_or_else(|| req.base.clone());
        if req.tmpfs {
            let tmpfs = tmpfs::create_layers(&req.name, fork_time_ns)?;
            layers.upper = tmpfs.upper;
            layers.work = tmpfs.work;
            layers.tmpfs = true;
        }
        let cleanup = |layers: &OverlayState| {
            if layers.tmpfs {
                let _ = tmpfs::remove_layers(&layers.upper);
            }
            let _ = fs::remove_dir_all(&root);
        };
        for path in [&layers.upper, &layers.work, &layers.merged] {
            if let Err(error) = fs::create_dir_all(path)
                .io_context(|| format!("failed to create workspace dir: {}", path.display()))
            {
                cleanup(&layers);
                return Err(error);
            }
        }

        let mounted = trace::time("materialize", || {
            mount_overlay(&lower, &layers.upper, &layers.work, &layers.merged)
        });
        if let Err(error) = mounted {
            mount::forget_probe();
            cleanup(&layers);
            return Err(error);
        }
        let recorded = trace::time("manifest", || {
            record_manifest(&req.name, frozen.as_ref(), &lower, fork_time_ns)
        });
        if let Err(error) = recorded {
            let _ = unmount_overlay(&layers.merged);
            let _ = crate::changes::manifest::remove(&req.name);
            cleanup(&layers);
            return Err(error);
        }
        apply_forkspaceclean(&layers.merged);

        Ok(ViewInfo {
            view_path: layers.merged.clone(),
            fork_time_ns,
            backend_state: BackendState::Overlay(layers),
            manifest_status: ManifestStatus::Ready,
            degraded: false,
            probe_reason: frozen.as_ref().and_then(describe_freeze),
        })
    }

    fn destroy(&self, ws: &Workspace, _mode: DestroyMode) -> Result<()> {
        let state = ws.linux_state()?;
        unmount_overlay(&state.merged)?;
        if state.tmpfs {
            tmpfs::remove_layers(&state.upper)?;
        }
        let root = paths::overlay_root(&ws.name);
        match fs::remove_dir_all(&root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result.io_context(|| {
                format!(
                    "remove workspace directory {}; retry `forkspace drop {}` after closing processes using it",
                    root.display(),
                    ws.name
                )
            }),
        }
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
        let _scan = trace::span("scan");
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

    fn ensure_view(&self, ws: &Workspace) -> Result {
        let state = ws.linux_state()?;
        match mount::view_health(&state.merged) {
            ViewHealth::Mounted | ViewHealth::Unknown => return Ok(()),
            ViewHealth::Disconnected => mount::detach_dead_mount(&state.merged)?,
            ViewHealth::Unmounted => {}
        }
        remount(ws, state)
    }
}

/// Mount an existing workspace's layers again, after a reboot or a dead fuse-overlayfs.
///
/// Refuses when the layers are gone (a `--tmpfs` workspace after a reboot) or when the bare
/// mount point holds files: those were written while the view was unmounted, so they are not
/// part of the workspace and mounting over them would hide them.
fn remount(ws: &Workspace, state: &OverlayState) -> Result {
    for (kind, path) in [("upper", &state.upper), ("work", &state.work)] {
        if !path.is_dir() {
            let kind = if state.tmpfs {
                format!("{kind} layer (on tmpfs, so lost at reboot; drop the workspace)")
            } else {
                kind.to_string()
            };
            return Err(crate::error::ForkspaceError::RecordedPathMissing {
                workspace: ws.name.clone(),
                kind,
                path: path.display().to_string(),
            });
        }
    }
    let stray = fs::read_dir(&state.merged)
        .io_context(|| format!("inspect {}", state.merged.display()))?
        .next()
        .is_some();
    if stray {
        return Err(crate::error::ForkspaceError::MountFailed(format!(
            "workspace '{}' is not mounted, and {} holds files written while it was unmounted; \
             move them out (they are not part of the workspace) and retry",
            ws.name,
            state.merged.display()
        )));
    }
    if let Some(lower) = &state.lower {
        if !lower.is_dir() {
            return Err(crate::error::ForkspaceError::RecordedPathMissing {
                workspace: ws.name.clone(),
                kind: "frozen copy".to_string(),
                path: lower.display().to_string(),
            });
        }
    }
    let lower = state.lower.as_deref().unwrap_or(&ws.base);
    mount_overlay(lower, &state.upper, &state.work, &state.merged)?;
    eprintln!("note: remounted workspace '{}'", ws.name);
    Ok(())
}
