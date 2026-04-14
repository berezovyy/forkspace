use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::backend::{BackendKind, BackendState, LinuxBackendState};
use crate::error::{ForkspaceError, Result};
use crate::util::forkspace_home;
use crate::workspace::Workspace;

const CURRENT_VERSION: u32 = 2;

fn default_version() -> u32 {
    CURRENT_VERSION
}

#[cfg(test)]
static PARENT_DIRECTORY_SYNCED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn sync_parent_directory(path: &std::path::Path) -> Result {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| ForkspaceError::Io {
            context: format!("sync state directory {}", path.display()),
            source: e,
        })?;
    #[cfg(test)]
    PARENT_DIRECTORY_SYNCED.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub listen_port: u16,
    pub active_workspace: Option<String>,
    pub target_port: Option<u16>,
    #[serde(default)]
    pub proxy_pid: Option<u32>,
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
    #[serde(flatten)]
    pub unknown_fields: Map<String, Value>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            workspaces: HashMap::new(),
            proxy: None,
            unknown_fields: Map::new(),
        }
    }
}

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
    base: std::path::PathBuf,
    upper: std::path::PathBuf,
    work: std::path::PathBuf,
    merged: std::path::PathBuf,
    parent: Option<String>,
    created: String,
    session: Option<String>,
    #[serde(default)]
    tmpfs: bool,
    #[serde(default)]
    tmpfs_size: Option<String>,
    #[serde(default)]
    ephemeral: bool,
    #[serde(default)]
    process: Option<crate::workspace::RunningProcess>,
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
            ephemeral: value.ephemeral,
            process: value.process,
            view_path: value.merged.clone(),
            fork_time_ns: 0,
            backend_kind: BackendKind::Linux,
            backend_state: BackendState::Linux(LinuxBackendState {
                upper: value.upper,
                work: value.work,
                merged: value.merged,
                tmpfs: value.tmpfs,
                tmpfs_size: value.tmpfs_size,
                unknown_fields: Map::new(),
            }),
            unknown_fields: value.unknown_fields,
        }
    }
}

fn migration_paths(path: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    (parent.join("state.v1.bak"), parent.join(".migrating"))
}

fn recover_interrupted_migration(path: &std::path::Path) -> Result {
    let (backup, marker) = migration_paths(path);
    if !marker.exists() {
        return Ok(());
    }
    if backup.exists() {
        fs::copy(&backup, path).map_err(|source| ForkspaceError::Io {
            context: format!(
                "restore interrupted state migration from {}",
                backup.display()
            ),
            source,
        })?;
        fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|source| ForkspaceError::Io {
                context: format!("sync restored state file {}", path.display()),
                source,
            })?;
    }
    fs::remove_file(&marker).map_err(|source| ForkspaceError::Io {
        context: format!("remove migration marker {}", marker.display()),
        source,
    })?;
    sync_parent_directory(path.parent().unwrap_or_else(|| std::path::Path::new(".")))
}

fn migrate_v1(path: &std::path::Path, content: &str, legacy: LegacyState) -> Result<State> {
    let (backup, marker) = migration_paths(path);
    fs::write(&marker, b"1 -> 2\n").map_err(|source| ForkspaceError::Io {
        context: format!("create migration marker {}", marker.display()),
        source,
    })?;
    fs::File::open(&marker)
        .and_then(|file| file.sync_all())
        .map_err(|source| ForkspaceError::Io {
            context: format!("sync migration marker {}", marker.display()),
            source,
        })?;

    let backup_tmp = backup.with_extension("bak.tmp");
    fs::write(&backup_tmp, content).map_err(|source| ForkspaceError::Io {
        context: format!("write state migration backup {}", backup_tmp.display()),
        source,
    })?;
    fs::File::open(&backup_tmp)
        .and_then(|file| file.sync_all())
        .map_err(|source| ForkspaceError::Io {
            context: format!("sync state migration backup {}", backup_tmp.display()),
            source,
        })?;
    fs::rename(&backup_tmp, &backup).map_err(|source| ForkspaceError::Io {
        context: format!("install state migration backup {}", backup.display()),
        source,
    })?;

    let state = State {
        version: CURRENT_VERSION,
        workspaces: legacy
            .workspaces
            .into_iter()
            .map(|(name, workspace)| (name, workspace.into()))
            .collect(),
        proxy: legacy.proxy,
        unknown_fields: legacy.unknown_fields,
    };
    state.save()?;
    fs::remove_file(&marker).map_err(|source| ForkspaceError::Io {
        context: format!("remove migration marker {}", marker.display()),
        source,
    })?;
    sync_parent_directory(path.parent().unwrap_or_else(|| std::path::Path::new(".")))?;
    Ok(state)
}

impl State {
    pub fn load() -> Result<State> {
        let path = forkspace_home().join("state.json");
        recover_interrupted_migration(&path)?;
        if !path.exists() {
            return Ok(State::default());
        }

        let content = fs::read_to_string(&path).map_err(|e| ForkspaceError::Io {
            context: format!("read state file {}", path.display()),
            source: e,
        })?;

        if content.trim().is_empty() {
            return Ok(State::default());
        }

        let value: Value = serde_json::from_str(&content).map_err(|e| {
            ForkspaceError::StateCorrupted(format!("invalid JSON in state file: {e}"))
        })?;
        let version = value.get("version").and_then(Value::as_u64).unwrap_or(1) as u32;

        if version <= 1 {
            let legacy: LegacyState = serde_json::from_value(value).map_err(|e| {
                ForkspaceError::StateCorrupted(format!("invalid version 1 state file: {e}"))
            })?;
            return migrate_v1(&path, &content, legacy);
        }

        let state: State = serde_json::from_value(value).map_err(|e| {
            ForkspaceError::StateCorrupted(format!("invalid JSON in state file: {e}"))
        })?;

        if state.version > CURRENT_VERSION {
            eprintln!(
                "warning: state file has version {}, this binary only knows version {}",
                state.version, CURRENT_VERSION
            );
        }

        Ok(state)
    }

    pub fn save(&self) -> Result {
        if self.version > CURRENT_VERSION {
            return Err(ForkspaceError::StateFailed(format!(
                "refusing to overwrite state version {} with a version {} binary; upgrade forkspace and retry",
                self.version, CURRENT_VERSION
            )));
        }

        let path = forkspace_home().join("state.json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ForkspaceError::Io {
                context: format!("create state directory {}", parent.display()),
                source: e,
            })?;
        }

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| ForkspaceError::StateFailed(format!("failed to serialize state: {e}")))?;

        let tmp_path = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp_path).map_err(|e| ForkspaceError::Io {
            context: format!("create temp state file {}", tmp_path.display()),
            source: e,
        })?;
        file.write_all(json.as_bytes())
            .map_err(|e| ForkspaceError::Io {
                context: format!("write state file {}", tmp_path.display()),
                source: e,
            })?;
        file.sync_all().map_err(|e| ForkspaceError::Io {
            context: format!("sync state file {}", tmp_path.display()),
            source: e,
        })?;
        fs::rename(&tmp_path, &path).map_err(|e| ForkspaceError::Io {
            context: format!("rename {} to {}", tmp_path.display(), path.display()),
            source: e,
        })?;
        let parent = path.parent().ok_or_else(|| {
            ForkspaceError::StateFailed("state file has no parent directory".to_string())
        })?;
        sync_parent_directory(parent)?;

        Ok(())
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

    pub fn with_lock<F, R>(f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R>,
    {
        let lock_path = forkspace_home().join("state.lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|e| ForkspaceError::Io {
                context: format!("create lock directory {}", parent.display()),
                source: e,
            })?;
        }

        let lock_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| ForkspaceError::Io {
                context: format!("open lock file {}", lock_path.display()),
                source: e,
            })?;

        let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            return Err(ForkspaceError::LockFailed(format!(
                "failed to acquire lock: {}",
                std::io::Error::last_os_error()
            )));
        }

        let result = f();

        drop(lock_file);

        result
    }

    pub fn with_state<F, R>(f: F) -> Result<R>
    where
        F: FnOnce(&mut State) -> Result<R>,
    {
        State::with_lock(|| {
            let mut state = State::load()?;
            let result = f(&mut state)?;
            state.save()?;
            Ok(result)
        })
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
    use super::{State, PARENT_DIRECTORY_SYNCED};
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
