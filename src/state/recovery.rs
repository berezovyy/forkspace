//! Crash recovery run on every state load: reconcile `state.json` with what is on disk.
//!
//! Each sweep is bounded so a load never does unbounded work; leftovers are picked up by
//! the next command.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{State, TrashRecord};
use crate::backend::overlay::frozen;
use crate::changes::manifest;
use crate::error::Result;
use crate::paths;
use crate::process::watch::is_alive;
use crate::sync::journal;
use crate::util::{now_unix_ns, IoContext};

/// Upper bound on directory entries inspected by one sweep.
const SWEEP_LIMIT: usize = 128;

impl State {
    /// Bring the state back in line with the disk after a crash. Returns whether the state
    /// changed and needs saving.
    ///
    /// The sweeps run in dependency order: trashing partial creates and forgetting
    /// interrupted drops happen before orphaned manifests are judged against the surviving
    /// names, and trash discovery happens before pruning.
    pub(crate) fn reconcile_runtime(&mut self) -> Result<bool> {
        let trash_root = paths::trash_dir();
        fs::create_dir_all(&trash_root)
            .io_context(|| format!("create trash directory {}", trash_root.display()))?;
        crate::disk::exclude_from_backups(&trash_root);

        let mut changed = journal::sweep_terminal()?;
        changed |= self.trash_abandoned_creates(&trash_root);
        changed |= self.forget_interrupted_drops(&trash_root);
        changed |= self.trash_unused_frozen_copies();
        changed |= self.remove_orphan_manifests();
        self.remove_orphan_ancestors();
        changed |= self.adopt_untracked_trash(&trash_root);
        changed |= self.forget_deleted_trash();
        Ok(changed)
    }

    /// A pending create whose creator died is moved to the trash, so no half-built view
    /// lingers under a name that could be reused.
    fn trash_abandoned_creates(&mut self, trash_root: &Path) -> bool {
        let mut changed = false;
        let mut retained = Vec::new();
        for pending in std::mem::take(&mut self.pending_creates) {
            if is_alive(pending.pid, pending.identity.as_deref()) {
                retained.push(pending);
                continue;
            }
            if !pending.path.exists() {
                changed = true;
                continue;
            }
            let destination = trash_root.join(format!(
                "partial-{}-{}-{}",
                pending.name,
                now_unix_ns(),
                std::process::id()
            ));
            match fs::rename(&pending.path, &destination) {
                Ok(()) => {
                    self.trash.push(TrashRecord::new(destination));
                    changed = true;
                }
                // Keep the intent so the next load retries the move.
                Err(_) => retained.push(pending),
            }
        }
        self.pending_creates = retained;
        changed
    }

    /// Frozen copies that no workspace mounts or measures against are moved to the trash.
    /// Skipped while a create is in flight, since the copy it just made is not recorded yet.
    pub(crate) fn trash_unused_frozen_copies(&mut self) -> bool {
        let trash_root = paths::trash_dir();
        let mut changed = false;
        for path in self.unused_frozen_copies() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let destination = trash_root.join(format!(
                "frozen-{}-{}-{}",
                name.trim_start_matches('.'),
                now_unix_ns(),
                std::process::id()
            ));
            if fs::create_dir_all(&trash_root).is_ok() && fs::rename(&path, &destination).is_ok() {
                self.trash.push(TrashRecord::new(destination));
                changed = true;
            }
        }
        changed
    }

    /// Frozen copies no workspace mounts or measures against (none while a create is in
    /// flight, since the copy it just made is not recorded yet).
    pub fn unused_frozen_copies(&self) -> Vec<PathBuf> {
        if !self.pending_creates.is_empty() {
            return Vec::new();
        }
        let referenced = frozen::ids_of(
            self.workspaces
                .values()
                .filter_map(|workspace| workspace.linux_state().ok())
                .flat_map(|layers| [layers.lower.as_deref(), layers.ancestor.as_deref()])
                .flatten(),
        );
        frozen::unreferenced(&referenced)
    }

    /// A workspace whose view is gone but whose trashed view (`<name>-<fork_time_ns>-…`) is
    /// present was mid-drop when forkspace stopped; finish the drop by forgetting it.
    fn forget_interrupted_drops(&mut self, trash_root: &Path) -> bool {
        let interrupted: Vec<String> = self
            .workspaces
            .values()
            .filter(|workspace| !workspace.view_path.exists())
            .filter(|workspace| {
                let prefix = format!("{}-{}-", workspace.name, workspace.fork_time_ns);
                bounded_entries(trash_root).any(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&prefix))
                })
            })
            .map(|workspace| workspace.name.clone())
            .collect();
        for name in &interrupted {
            self.workspaces.remove(name);
            let _ = manifest::remove(name);
        }
        !interrupted.is_empty()
    }

    /// Manifests belong to a recorded or pending workspace; temporary manifests also need a
    /// live writer. Anything else is deleted.
    fn remove_orphan_manifests(&self) -> bool {
        let mut changed = false;
        for path in bounded_entries(&paths::manifest_dir()) {
            let Some(file) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(ManifestFile::parse)
            else {
                continue;
            };
            let owned = self.name_in_use(file.workspace);
            let abandoned =
                !file.installed && !file.writer_pid.is_some_and(|pid| is_alive(pid, None));
            if (abandoned || !owned) && fs::remove_file(&path).is_ok() {
                changed = true;
            }
        }
        changed
    }

    /// Checkpoints and fork-time copies belong to a recorded or pending workspace; staging
    /// copies need a live writer.
    fn remove_orphan_ancestors(&self) {
        for path in bounded_entries(&paths::checkpoint_root()) {
            let owned = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| self.name_in_use(name));
            if !owned {
                let _ = fs::remove_dir_all(&path);
            }
        }
        for path in bounded_entries(&paths::ancestor_root()) {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let orphan = match name
                .strip_prefix('.')
                .and_then(|rest| rest.rsplit_once(".tmp-"))
            {
                Some((_, pid)) => !pid.parse().is_ok_and(|pid| is_alive(pid, None)),
                None => !self.name_in_use(name),
            };
            if orphan {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }

    /// Every directory in the trash is tracked, so deletions interrupted before they were
    /// recorded are still retried.
    fn adopt_untracked_trash(&mut self, trash_root: &Path) -> bool {
        let mut changed = false;
        for path in bounded_entries(trash_root) {
            if !self.trash.iter().any(|record| record.path == path) {
                self.trash.push(TrashRecord::new(path));
                changed = true;
            }
        }
        changed
    }

    /// Trash records whose directory has been deleted are dropped.
    fn forget_deleted_trash(&mut self) -> bool {
        let before = self.trash.len();
        self.trash.retain(|record| {
            record.path.exists()
                && record
                    .path
                    .file_name()
                    .is_none_or(|name| name != "CACHEDIR.TAG")
        });
        self.trash.len() != before
    }
}

/// A file in the manifest directory, classified by its name.
struct ManifestFile<'a> {
    workspace: &'a str,
    /// The writing process, for in-progress `.<name>.jsonl.tmp-<pid>` files.
    writer_pid: Option<u32>,
    /// `<name>.jsonl`, as opposed to a temporary or staged (`.jsonl.new`) file.
    installed: bool,
}

impl<'a> ManifestFile<'a> {
    fn parse(file_name: &'a str) -> Option<Self> {
        if let Some((workspace, pid)) = file_name
            .strip_prefix('.')
            .and_then(|name| name.split_once(".jsonl.tmp-"))
        {
            return Some(Self {
                workspace,
                writer_pid: pid.parse().ok(),
                installed: false,
            });
        }
        if let Some(workspace) = file_name.strip_suffix(".jsonl.new") {
            return Some(Self {
                workspace,
                writer_pid: None,
                installed: false,
            });
        }
        file_name.strip_suffix(".jsonl").map(|workspace| Self {
            workspace,
            writer_pid: None,
            installed: true,
        })
    }
}

/// At most [`SWEEP_LIMIT`] entries of `dir`, skipping unreadable ones and the backup
/// exclusion tag; empty if `dir` cannot be read.
fn bounded_entries(dir: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.file_name().is_none_or(|name| name != "CACHEDIR.TAG"))
        .take(SWEEP_LIMIT)
}

/// Delete trashed directories in the background; a crash mid-delete just leaves them for
/// the next sweep.
pub fn start_trash_removals() {
    for path in bounded_entries(&paths::trash_dir()) {
        let _ = Command::new("rm")
            .args(["-rf", "--"])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}
