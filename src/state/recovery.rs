//! Crash recovery run on every state load: reconcile `state.json` with what is on disk.
//!
//! Each sweep is bounded so a load never does unbounded work; leftovers are picked up by
//! the next command.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{State, TrashRecord};
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

        let mut changed = journal::sweep_terminal()?;
        changed |= self.trash_abandoned_creates(&trash_root);
        changed |= self.forget_interrupted_drops(&trash_root);
        changed |= self.remove_orphan_manifests();
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
        self.trash.retain(|record| record.path.exists());
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

/// At most [`SWEEP_LIMIT`] entries of `dir`, skipping unreadable ones; empty if `dir`
/// cannot be read.
fn bounded_entries(dir: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .take(SWEEP_LIMIT)
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
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
