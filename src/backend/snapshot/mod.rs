//! Directory-snapshot backends: the workspace is an ordinary directory materialized by a
//! [`ForkPrimitive`] (APFS clone, per-file reflink, or plain copy). Changes are found by
//! comparing the view against the fork manifest.
//!
//! [`SnapshotBackend`] is primitive-agnostic; everything that differs between primitives
//! (capabilities, availability, how the fork is reported) lives behind [`ForkPrimitive`].

#[cfg(target_os = "macos")]
mod clonefile;
mod copy;
#[cfg(target_os = "linux")]
mod reflink;
mod xattr;

#[cfg(target_os = "macos")]
pub(crate) use clonefile::ClonefilePrimitive;
pub(crate) use copy::CopyPrimitive;
#[cfg(target_os = "linux")]
pub(crate) use reflink::{probe as reflink_probe, ReflinkPrimitive};

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Map;

use super::{
    BackendKind, BackendState, Capabilities, ChangeCount, CreateReq, DestroyMode, ViewInfo,
    WorkspaceBackend,
};
use crate::changes::manifest::{self, ManifestStatus};
use crate::changes::FileChange;
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::util::fs::ctime_ns;
use crate::util::{now_unix_ns, IoContext};
use crate::workspace::Workspace;

/// Capabilities shared by the per-file primitives (reflink and plain copy).
const DIRECTORY_COPY_CAPABILITIES: Capabilities = Capabilities {
    tmpfs_backing: false,
    network_namespace: true,
    instant_fork: false,
    sealed_parent: false,
};

/// Counters gathered while materializing a view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ForkStats {
    pub reflinked_files: u64,
    pub copied_files: u64,
    pub copied_bytes: u64,
    /// Latest ctime among materialized entries.
    pub max_ctime_ns: u128,
}

impl ForkStats {
    pub(crate) fn observe(&mut self, metadata: &fs::Metadata) {
        let ctime = u128::try_from(ctime_ns(metadata)).unwrap_or(0);
        self.max_ctime_ns = self.max_ctime_ns.max(ctime);
    }
}

/// How a finished fork is reported to the user.
#[derive(Debug, Clone, Default)]
pub(crate) struct ForkSummary {
    pub degraded: bool,
    pub note: Option<String>,
}

impl ForkSummary {
    fn record_manifest_failure(&mut self, error: &ForkspaceError) {
        self.degraded = true;
        let tracking = format!(
            "change tracking is degraded because the fork manifest could not be written: {error}"
        );
        self.note = Some(match self.note.take() {
            Some(note) => format!("{note}; {tracking}"),
            None => tracking,
        });
    }
}

/// A way of turning a base directory into an independent workspace directory.
pub(crate) trait ForkPrimitive: Send + Sync + 'static {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> &'static Capabilities;
    /// Fails with a user-facing diagnosis when `base` cannot be forked into `dest_parent`.
    fn check_available(&self, base: &Path, dest_parent: &Path) -> Result;
    /// Creates `dest` as a copy of `base`. On error the caller removes `dest`.
    fn materialize(&self, base: &Path, dest: &Path) -> Result<ForkStats>;
    fn summarize(&self, stats: &ForkStats, elapsed_ms: u128) -> ForkSummary;

    /// The instant changes are measured from, given when materializing `started_ns` and what
    /// it produced. Per-file copies stamp every entry with a fresh ctime, so by default the
    /// baseline is placed after the copy and after every ctime it produced.
    fn fork_time_ns(&self, _started_ns: u128, stats: &ForkStats) -> u128 {
        now_unix_ns().max(stats.max_ctime_ns).saturating_add(1)
    }

    /// Whether the fork manifest is built by a background worker instead of inline.
    fn manifest_in_background(&self) -> bool {
        false
    }

    fn backend_state(&self, stats: &ForkStats) -> BackendState {
        BackendState::CowDir {
            reflinked_files: stats.reflinked_files,
            copied_files: stats.copied_files,
            unknown_fields: Map::new(),
        }
    }

    /// The error for a `--tmpfs` request, which no directory primitive supports.
    fn tmpfs_unsupported(&self) -> ForkspaceError {
        ForkspaceError::CapabilityUnsupported {
            diagnosis: "--tmpfs is unavailable for directory-copy backends".to_string(),
            remedy: "omit --tmpfs or install fuse-overlayfs".to_string(),
        }
    }
}

pub(crate) struct SnapshotBackend<P: ForkPrimitive> {
    primitive: P,
}

impl<P: ForkPrimitive> SnapshotBackend<P> {
    pub(crate) fn new(primitive: P) -> Self {
        Self { primitive }
    }

    /// Records the fork manifest, inline or in the background as the primitive prefers.
    fn start_manifest(
        &self,
        name: &str,
        view: &Path,
        fork_time_ns: u128,
    ) -> Result<ManifestStatus> {
        if self.primitive.manifest_in_background() {
            manifest::spawn(name.to_string(), view.to_path_buf(), fork_time_ns)?;
            Ok(ManifestStatus::Building)
        } else {
            manifest::build(name, view, fork_time_ns)?;
            Ok(ManifestStatus::Ready)
        }
    }
}

impl<P: ForkPrimitive> WorkspaceBackend for SnapshotBackend<P> {
    fn kind(&self) -> BackendKind {
        self.primitive.kind()
    }

    fn capabilities(&self) -> &Capabilities {
        self.primitive.capabilities()
    }

    fn preflight(&self, base: &Path) -> Result {
        if !base.is_dir() {
            return Err(ForkspaceError::InvalidArgument(format!(
                "base path {} is not a directory",
                base.display()
            )));
        }
        let root = paths::snapshot_root();
        fs::create_dir_all(&root).io_context(|| format!("create {}", root.display()))?;
        let base = base
            .canonicalize()
            .io_context(|| format!("canonicalize {}", base.display()))?;
        let root = root
            .canonicalize()
            .io_context(|| format!("canonicalize {}", root.display()))?;
        if root.starts_with(&base) {
            return Err(ForkspaceError::CapabilityUnsupported {
                diagnosis: "workspace root is inside the project tree".to_string(),
                remedy: "set FORKSPACE_HOME outside the project and retry".to_string(),
            });
        }
        self.primitive.check_available(&base, &root)
    }

    fn create(&self, req: &CreateReq) -> Result<ViewInfo> {
        if req.tmpfs {
            return Err(self.primitive.tmpfs_unsupported());
        }
        let destination = paths::snapshot_dir(&req.name);
        if destination.exists() {
            return Err(ForkspaceError::WorkspaceExists(req.name.clone()));
        }

        let started = Instant::now();
        let started_ns = now_unix_ns();
        let stats = self
            .primitive
            .materialize(&req.base, &destination)
            .inspect_err(|_| {
                let _ = fs::remove_dir_all(&destination);
            })?;
        let fork_time_ns = self.primitive.fork_time_ns(started_ns, &stats);
        let manifest = self.start_manifest(&req.name, &destination, fork_time_ns);

        let mut summary = self
            .primitive
            .summarize(&stats, started.elapsed().as_millis());
        let manifest_status = manifest.unwrap_or_else(|error| {
            summary.record_manifest_failure(&error);
            ManifestStatus::Degraded
        });
        Ok(ViewInfo {
            view_path: destination,
            fork_time_ns,
            backend_state: self.primitive.backend_state(&stats),
            manifest_status,
            degraded: summary.degraded,
            probe_reason: summary.note,
        })
    }

    /// Moves the view into the trash directory; the slow delete happens asynchronously.
    fn destroy(&self, ws: &Workspace, _mode: DestroyMode) -> Result {
        let trash_root = paths::trash_dir();
        fs::create_dir_all(&trash_root)
            .io_context(|| format!("create {}", trash_root.display()))?;
        let trash = trash_root.join(format!(
            "{}-{}-{}",
            ws.name,
            ws.fork_time_ns,
            std::process::id()
        ));
        fs::rename(&ws.view_path, &trash)
            .io_context(|| format!("move workspace '{}' to trash", ws.name))
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
        crate::changes::collect_clone_changes(ws, include_ignored, need_line_counts)
    }

    fn change_count(&self, ws: &Workspace, include_ignored: bool) -> Result<ChangeCount> {
        manifest::change_count(ws, include_ignored)
    }

    fn shell_env(&self, ws: &Workspace) -> Vec<(String, String)> {
        vec![("FORKSPACE_VIEW".to_string(), ws.view_path.display().to_string())]
    }
}
