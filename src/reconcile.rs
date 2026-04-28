use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{ForkspaceError, Result};
use crate::util::{forkspace_home, IoContext};

const SWEEP_LIMIT: usize = 128;

pub fn start_trash_removals() {
    let root = forkspace_home().join("trash");
    let Ok(entries) = fs::read_dir(&root) else {
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

fn journal_terminal(value: &serde_json::Value) -> bool {
    match value.get("op").and_then(|value| value.as_str()) {
        Some("merge") => {
            if let Some(phase) = value.get("phase").and_then(|value| value.as_str()) {
                return phase == "done";
            }
            let next = value.get("next").and_then(|value| value.as_u64());
            let total = value
                .get("changes")
                .and_then(|value| value.as_array())
                .map(|values| values.len() as u64);
            next.zip(total).is_some_and(|(next, total)| next >= total)
        }
        Some("pull") => value.get("phase").and_then(|value| value.as_str()) == Some("done"),
        _ => false,
    }
}

pub fn sweep_terminal_journals() -> Result<bool> {
    let root = forkspace_home().join("journal");
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(false);
    };
    let mut changed = false;
    for entry in entries.take(SWEEP_LIMIT) {
        let path = entry
            .io_context(|| format!("read journal directory {}", root.display()))?
            .path();
        if path.extension().and_then(|value| value.to_str()) == Some("tmp") {
            if fs::remove_file(&path).is_ok() {
                changed = true;
            }
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if journal_terminal(&value) && fs::remove_file(&path).is_ok() {
            changed = true;
        }
    }
    Ok(changed)
}

fn merge_journal(workspace: &str) -> Result<Option<PathBuf>> {
    let root = forkspace_home().join("journal");
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(None);
    };
    let prefix = format!("merge-{workspace}-");
    for entry in entries.take(SWEEP_LIMIT) {
        let path = entry
            .io_context(|| format!("read journal directory {}", root.display()))?
            .path();
        if !path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.starts_with(&prefix) && value.ends_with(".json"))
        {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).io_context(|| format!("read {}", path.display()))?,
        )
        .map_err(|error| {
            ForkspaceError::StateCorrupted(format!("invalid merge journal: {error}"))
        })?;
        if !journal_terminal(&value) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

pub fn guard_merge(workspace: &str) -> Result {
    if merge_journal(workspace)?.is_some() {
        return Err(ForkspaceError::InterruptedOperation {
            workspace: workspace.to_string(),
            operation: "merge".to_string(),
            remedy: format!("run `forkspace merge {workspace} --resume`"),
        });
    }
    Ok(())
}

pub fn base_missing(base: &Path) -> bool {
    !base.is_dir()
}
