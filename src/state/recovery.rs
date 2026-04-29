//! Crash recovery run on every state load: reconcile `state.json` with what is on disk.
//!
//! Each sweep is bounded so a load never does unbounded work; leftovers are picked up by
//! the next command.

use std::fs;

use serde_json::Map;

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
    /// Returns whether the state changed and needs saving.
    pub(crate) fn reconcile_runtime(&mut self) -> Result<bool> {
        let trash_root = paths::trash_dir();
        fs::create_dir_all(&trash_root)
            .io_context(|| format!("create trash directory {}", trash_root.display()))?;
        let mut changed = false;
        changed |= journal::sweep_terminal()?;
        let mut retained = Vec::new();
        for pending in self.pending_creates.drain(..) {
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
                    self.trash.push(TrashRecord {
                        path: destination,
                        unknown_fields: Map::new(),
                    });
                    changed = true;
                }
                Err(_) => retained.push(pending),
            }
        }
        self.pending_creates = retained;

        let interrupted_drops: Vec<_> = self
            .workspaces
            .values()
            .filter(|workspace| {
                if workspace.view_path.exists() {
                    return false;
                }
                let prefix = format!("{}-{}-", workspace.name, workspace.fork_time_ns);
                fs::read_dir(&trash_root).is_ok_and(|entries| {
                    entries.take(SWEEP_LIMIT).flatten().any(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.starts_with(&prefix))
                    })
                })
            })
            .map(|workspace| workspace.name.clone())
            .collect();
        for name in interrupted_drops {
            self.workspaces.remove(&name);
            let _ = manifest::remove(&name);
            changed = true;
        }

        let manifest_root = paths::manifest_dir();
        if let Ok(entries) = fs::read_dir(&manifest_root) {
            for path in entries
                .take(SWEEP_LIMIT)
                .filter_map(|entry| entry.ok().map(|value| value.path()))
            {
                let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
                    continue;
                };
                let (name, writer_pid, installed) = if let Some(name) = file_name
                    .strip_prefix('.')
                    .and_then(|value| value.split_once(".jsonl.tmp-"))
                {
                    (name.0, name.1.parse::<u32>().ok(), false)
                } else if let Some(name) = file_name.strip_suffix(".jsonl.new") {
                    (name, None, false)
                } else if let Some(name) = file_name.strip_suffix(".jsonl") {
                    (name, None, true)
                } else {
                    continue;
                };
                let owned = self.workspaces.contains_key(name)
                    || self
                        .pending_creates
                        .iter()
                        .any(|pending| pending.name == name);
                let live_writer = writer_pid.is_some_and(|pid| is_alive(pid, None));
                if ((!installed && !live_writer) || !owned) && fs::remove_file(&path).is_ok() {
                    changed = true;
                }
            }
        }

        if let Ok(entries) = fs::read_dir(&trash_root) {
            for path in entries
                .take(SWEEP_LIMIT)
                .filter_map(|entry| entry.ok().map(|value| value.path()))
            {
                if !self.trash.iter().any(|record| record.path == path) {
                    self.trash.push(TrashRecord {
                        path,
                        unknown_fields: Map::new(),
                    });
                    changed = true;
                }
            }
        }

        self.trash.retain(|record| {
            if !record.path.exists() {
                changed = true;
                return false;
            }
            true
        });
        Ok(changed)
    }
}

/// Delete trashed directories in the background; a crash mid-delete just leaves them for
/// the next sweep.
pub fn start_trash_removals() {
    let Ok(entries) = fs::read_dir(paths::trash_dir()) else {
        return;
    };
    for path in entries
        .take(SWEEP_LIMIT)
        .filter_map(|entry| entry.ok().map(|value| value.path()))
    {
        let _ = std::process::Command::new("rm")
            .args(["-rf", "--"])
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}
