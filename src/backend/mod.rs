//! Workspace backends: how a workspace view is materialized, scanned, and destroyed.
//!
//! Every command dispatches through [`WorkspaceBackend`]. The backend kind is chosen once at
//! fork time (by capability probe, see [`select_backend_for_create`]) and persisted, so later
//! commands reopen the workspace with [`select_backend`] and the recorded kind.

pub mod overlay;
pub mod probe;
pub(crate) mod snapshot;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::changes::manifest::ManifestStatus;
use crate::changes::FileChange;
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::workspace::Workspace;

pub use overlay::OverlayState;

/// Static feature flags a backend advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Workspace processes can run in their own network namespace.
    pub network_namespace: bool,
    /// Forking costs O(1) regardless of project size.
    pub instant_fork: bool,
    /// The parent view stays read-only underneath its children.
    pub sealed_parent: bool,
}

/// The persisted identity of a backend. Serialized names are part of `state.json`.
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

    /// Human-readable platform whose builds can open workspaces of this kind.
    fn platform(self) -> &'static str {
        match self {
            Self::Overlay | Self::LinuxReflink => "Linux",
            Self::MacClone => "macOS",
            Self::PlainCopy => "any supported platform",
        }
    }

    /// Whether a child of this workspace is a full directory snapshot of its parent's view
    /// (rather than an overlay layered on top of it).
    pub fn stacking_is_snapshot(self) -> bool {
        matches!(self, Self::MacClone | Self::LinuxReflink | Self::PlainCopy)
    }
}

/// Backend-specific state persisted in `state.json`. Unknown fields round-trip untouched.
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

/// A request to materialize a new workspace view of `base`.
pub struct CreateReq {
    pub name: String,
    pub base: PathBuf,
    /// The layers of the overlay workspace `base` is the view of, when stacking.
    pub parent_layers: Option<OverlayState>,
}

/// What [`WorkspaceBackend::create`] produced.
pub struct ViewInfo {
    pub view_path: PathBuf,
    /// Files whose ctime is later than this were changed after the fork.
    pub fork_time_ns: u128,
    pub backend_state: BackendState,
    pub manifest_status: ManifestStatus,
    /// The fork succeeded but is slower or less precise than the platform's best.
    pub degraded: bool,
    /// How the view was materialized or where it fell short; shown to the user.
    pub probe_reason: Option<String>,
}

/// A change count that may not be computable yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeCount {
    Ready(usize),
    /// The fork manifest is still being built in the background.
    Pending,
    /// The fork manifest is missing, so changes cannot be counted cheaply.
    Degraded,
}

/// How [`WorkspaceBackend::destroy`] should tear a workspace down.
///
/// There is only one mode today; the parameter keeps room in the trait for others.
#[derive(Debug, Clone, Copy)]
pub enum DestroyMode {
    Normal,
}

/// The operations every workspace backend provides.
pub trait WorkspaceBackend {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> &Capabilities;
    /// Validates `base` before any state is recorded for a new workspace.
    fn preflight(&self, base: &Path) -> Result<()>;
    /// Materializes the workspace view described by `req`.
    fn create(&self, req: &CreateReq) -> Result<ViewInfo>;
    fn destroy(&self, ws: &Workspace, mode: DestroyMode) -> Result<()>;
    fn view_path(&self, ws: &Workspace) -> PathBuf;
    /// Changes made inside the workspace since it was forked.
    fn changes(
        &self,
        ws: &Workspace,
        include_ignored: bool,
        need_line_counts: bool,
    ) -> Result<Vec<FileChange>>;
    fn change_count(&self, ws: &Workspace, include_ignored: bool) -> Result<ChangeCount>;
    /// The changes that can move between the workspace and its base: everything except
    /// runtime and internal paths (`node_modules`, `.git`), ignored source files included.
    fn transferable_changes(&self, ws: &Workspace) -> Result<Vec<FileChange>> {
        Ok(self
            .changes(ws, true, false)?
            .into_iter()
            .filter(|change| crate::sync::is_transferable(change.class))
            .collect())
    }
    /// Changes made in the base since the workspace was forked.
    fn base_changes(&self, ws: &Workspace) -> Result<Vec<FileChange>> {
        crate::sync::pull::base_changes(ws)
    }
    /// Environment variables exported to processes run inside the workspace.
    fn shell_env(&self, ws: &Workspace) -> Vec<(String, String)>;
    /// Make the view usable before a command reads or writes through it, for example by
    /// remounting an overlay that a reboot unmounted. A view that is already usable is left
    /// alone.
    fn ensure_view(&self, _ws: &Workspace) -> Result {
        Ok(())
    }
}

/// Shared preflight: every backend requires the fork origin to be an existing directory.
fn require_directory(base: &Path) -> Result {
    if base.is_dir() {
        return Ok(());
    }
    Err(ForkspaceError::InvalidArgument(format!(
        "base path {} is not a directory",
        base.display()
    )))
}

/// Opens the backend for the `recorded` kind, or the platform default when no kind has been
/// recorded.
pub fn select_backend(recorded: Option<BackendKind>) -> Result<Box<dyn WorkspaceBackend>> {
    let kind = recorded.unwrap_or(PLATFORM_DEFAULT);
    instantiate(kind).ok_or_else(|| unavailable(kind))
}

/// Chooses the backend for a new workspace forked from `base`: a full copy when forced, and
/// otherwise the best primitive the platform offers.
pub fn select_backend_for_create(base: &Path) -> Result<Box<dyn WorkspaceBackend>> {
    Ok(choose_backend_for_create(base)?.0)
}

/// [`select_backend_for_create`], also saying why faster backends were passed over, when they
/// were: the probes' own reasons, such as `fuse-overlayfs: cannot open /dev/fuse: ...`.
pub fn choose_backend_for_create(
    base: &Path,
) -> Result<(Box<dyn WorkspaceBackend>, Option<String>)> {
    let (kind, skipped) = probe_create_kind(base)?;
    Ok((select_backend(Some(kind))?, skipped))
}

/// The best backend for `base` that copies it rather than layering over it: for a project
/// the overlay cannot hold (see [`ForkspaceError::ReservedOverlayName`]).
pub fn copying_backend_for(base: &Path) -> Result<Box<dyn WorkspaceBackend>> {
    let kind = if probe::reflink_supported(base, &paths::snapshot_root()) {
        BackendKind::LinuxReflink
    } else {
        BackendKind::PlainCopy
    };
    select_backend(Some(kind))
}

/// Where a workspace of `kind` named `name` will live. Recorded before creation so an
/// interrupted create can be cleaned up.
pub(crate) fn pending_create_path(kind: BackendKind, name: &str) -> PathBuf {
    match kind {
        BackendKind::Overlay => paths::overlay_root(name),
        BackendKind::MacClone | BackendKind::LinuxReflink | BackendKind::PlainCopy => {
            paths::snapshot_dir(name)
        }
    }
}

const PLATFORM_DEFAULT: BackendKind = if cfg!(target_os = "macos") {
    BackendKind::MacClone
} else {
    BackendKind::Overlay
};

/// The backend implementing `kind`, or `None` when this build cannot open it.
fn instantiate(kind: BackendKind) -> Option<Box<dyn WorkspaceBackend>> {
    use snapshot::SnapshotBackend;

    match kind {
        #[cfg(target_os = "linux")]
        BackendKind::Overlay => Some(Box::new(overlay::OverlayBackend::new())),
        #[cfg(target_os = "linux")]
        BackendKind::LinuxReflink => {
            Some(Box::new(SnapshotBackend::new(snapshot::ReflinkPrimitive)))
        }
        #[cfg(target_os = "macos")]
        BackendKind::MacClone => Some(Box::new(SnapshotBackend::new(snapshot::ClonefilePrimitive))),
        BackendKind::PlainCopy => Some(Box::new(SnapshotBackend::new(snapshot::CopyPrimitive))),
        _ => None,
    }
}

/// The best backend kind for forking `base` on this platform.
#[cfg(target_os = "linux")]
fn probe_create_kind(base: &Path) -> Result<(BackendKind, Option<String>)> {
    let report = probe::linux_report(base, &paths::snapshot_root());
    let kind = report
        .winner()
        .ok_or_else(|| ForkspaceError::BackendUnavailable {
            diagnosis: format!(
                "no workspace materialization primitive passed its capability probe ({})",
                report.skipped().unwrap_or_default()
            ),
            remedy: "ensure FORKSPACE_HOME is writable and retry".to_string(),
        })?;
    Ok((kind, report.skipped()))
}

/// The best backend kind for forking `base` on this platform: APFS clones, or a copy when
/// the project and `FORKSPACE_HOME` are not on one APFS volume.
#[cfg(target_os = "macos")]
fn probe_create_kind(base: &Path) -> Result<(BackendKind, Option<String>)> {
    use snapshot::ForkPrimitive;

    let root = paths::snapshot_root();
    let _ = std::fs::create_dir_all(&root);
    match snapshot::ClonefilePrimitive.check_available(base, &root) {
        Ok(()) => Ok((BackendKind::MacClone, None)),
        Err(error) => Ok((
            BackendKind::PlainCopy,
            Some(format!("APFS clones: {error}")),
        )),
    }
}

/// The best backend kind for forking `base` on this platform.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn probe_create_kind(_base: &Path) -> Result<(BackendKind, Option<String>)> {
    Ok((BackendKind::PlainCopy, None))
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
