//! The durable control plane: `state.json`, guarded by an exclusive `flock`.
//!
//! Every mutation goes through [`State::with_state`], which takes the lock, loads (running
//! migration and crash recovery), applies the closure, and saves atomically.

mod migrate;
mod recovery;

use std::collections::HashMap;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::backend::BackendKind;
use crate::error::{ForkspaceError, Result};
use crate::paths;
use crate::util::{write_atomic, IoContext};
use crate::workspace::Workspace;

pub use recovery::start_trash_removals;

const CURRENT_VERSION: u32 = 2;

fn default_version() -> u32 {
    CURRENT_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub listen_port: u16,
    pub active_workspace: Option<String>,
    pub target_port: Option<u16>,
    #[serde(default)]
    pub proxy_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_identity: Option<String>,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingCreate {
    pub name: String,
    pub path: PathBuf,
    pub backend_kind: BackendKind,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashRecord {
    pub path: PathBuf,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub workspaces: HashMap<String, Workspace>,
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pending_creates: Vec<PendingCreate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trash: Vec<TrashRecord>,
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            workspaces: HashMap::new(),
            proxy: None,
            pending_creates: Vec::new(),
            trash: Vec::new(),
            unknown_fields: Map::new(),
        }
    }
}

impl State {
    fn load_unlocked() -> Result<State> {
        let path = paths::state_file();
        migrate::recover_interrupted(&path)?;
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(ForkspaceError::Io {
                    context: format!("read state file {}", path.display()),
                    source: error,
                })
            }
        };

        let mut state = if content.trim().is_empty() {
            State::default()
        } else {
            let value: Value = serde_json::from_str(&content).map_err(|e| {
                ForkspaceError::StateCorrupted(format!("invalid JSON in state file: {e}"))
            })?;
            let version = value.get("version").and_then(Value::as_u64).unwrap_or(1) as u32;
            if version <= 1 {
                return migrate::migrate_v1(&path, &content, value);
            }
            serde_json::from_value(value).map_err(|e| {
                ForkspaceError::StateCorrupted(format!("invalid JSON in state file: {e}"))
            })?
        };

        if state.version > CURRENT_VERSION {
            eprintln!(
                "warning: state file has version {}, this binary only knows version {}",
                state.version, CURRENT_VERSION
            );
        } else if state.reconcile_runtime()? {
            state.save()?;
        }
        Ok(state)
    }

    /// Read a consistent snapshot. Use [`State::with_state`] to mutate.
    pub fn load() -> Result<State> {
        let state = Self::with_lock(Self::load_unlocked)?;
        start_trash_removals();
        Ok(state)
    }

    pub fn save(&self) -> Result {
        if self.version > CURRENT_VERSION {
            return Err(ForkspaceError::StateFailed(format!(
                "refusing to overwrite state version {} with a version {} binary; upgrade forkspace and retry",
                self.version, CURRENT_VERSION
            )));
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| ForkspaceError::StateFailed(format!("failed to serialize state: {e}")))?;
        write_atomic(&paths::state_file(), json.as_bytes())
    }

    /// Run `f` while holding the exclusive state lock.
    pub fn with_lock<F, R>(f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R>,
    {
        let lock_path = paths::state_lock();
        crate::util::ensure_parent_dir(&lock_path)?;
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .io_context(|| format!("open lock file {}", lock_path.display()))?;

        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(ForkspaceError::LockFailed(format!(
                "failed to acquire lock: {}",
                std::io::Error::last_os_error()
            )));
        }
        // The lock is released when `lock_file` drops.
        f()
    }

    /// Load, mutate, and save under the lock. Nothing is saved if `f` fails.
    pub fn with_state<F, R>(f: F) -> Result<R>
    where
        F: FnOnce(&mut State) -> Result<R>,
    {
        let result = State::with_lock(|| {
            let mut state = State::load_unlocked()?;
            let result = f(&mut state)?;
            state.save()?;
            Ok(result)
        })?;
        start_trash_removals();
        Ok(result)
    }

    pub fn add_workspace(&mut self, ws: Workspace) -> Result {
        if self.workspaces.contains_key(&ws.name) {
            return Err(ForkspaceError::WorkspaceExists(ws.name.clone()));
        }
        self.workspaces.insert(ws.name.clone(), ws);
        Ok(())
    }

    pub fn remove_workspace(&mut self, name: &str) -> Result {
        if self.workspaces.remove(name).is_none() {
            return Err(ForkspaceError::WorkspaceNotFound(name.to_string()));
        }
        Ok(())
    }

    pub(crate) fn has_pending_create(&self, name: &str) -> bool {
        self.pending_creates
            .iter()
            .any(|pending| pending.name == name)
    }

    pub fn children_of(&self, name: &str) -> Vec<&Workspace> {
        self.workspaces
            .values()
            .filter(|ws| ws.parent.as_deref() == Some(name))
            .collect()
    }

    pub fn parent_chain(&self, name: &str) -> Vec<&Workspace> {
        let mut chain = Vec::new();
        let mut current = name;

        while let Some(ws) = self.workspaces.get(current) {
            chain.push(ws);
            match &ws.parent {
                Some(parent) => current = parent,
                None => break,
            }
        }

        chain.reverse();
        chain
    }

    pub fn require_workspace(&self, name: &str) -> Result<&Workspace> {
        self.workspaces
            .get(name)
            .ok_or_else(|| ForkspaceError::WorkspaceNotFound(name.to_string()))
    }

    pub fn require_workspace_mut(&mut self, name: &str) -> Result<&mut Workspace> {
        self.workspaces
            .get_mut(name)
            .ok_or_else(|| ForkspaceError::WorkspaceNotFound(name.to_string()))
    }

    pub fn depth_of(&self, name: &str) -> usize {
        let mut depth = 0;
        let mut current = name;
        while let Some(ws) = self.workspaces.get(current) {
            match ws.parent.as_deref() {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                }
                None => break,
            }
        }
        depth
    }

    pub fn workspace_names(&self) -> Vec<String> {
        self.workspaces.keys().cloned().collect()
    }

    pub fn sorted_deepest_first(&self, names: &mut [String]) {
        names.sort_by_key(|name| std::cmp::Reverse(self.depth_of(name)));
    }
}

#[cfg(test)]
mod tests {
    use super::State;
    use crate::util::fs::PARENT_DIRECTORY_SYNCED;
    use std::sync::atomic::Ordering;

    #[test]
    fn save_syncs_parent_directory() {
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("FORKSPACE_HOME", home.path()) };
        PARENT_DIRECTORY_SYNCED.store(false, Ordering::SeqCst);

        State::default().save().unwrap();

        assert!(PARENT_DIRECTORY_SYNCED.load(Ordering::SeqCst));
    }
}
