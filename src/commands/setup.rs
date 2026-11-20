//! `forkspace setup`: make the coding agents on this machine use forkspace. Today that is Claude
//! Code (`~/.claude`): it gets the forkspace skill, and worktree hooks so that its worktrees
//! (`claude --worktree`, isolated subagents) are forkspace workspaces. Safe to run again.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{ForkspaceError, Result};
use crate::util::IoContext;

pub fn exec() -> Result {
    let Some(claude) = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"))
    else {
        println!("no coding agent found: HOME is not set; nothing to set up");
        return Ok(());
    };
    if !claude.is_dir() {
        println!(
            "no coding agent found (Claude Code would be in {}); nothing to set up",
            claude.display()
        );
        return Ok(());
    }
    let mut added = Vec::new();
    if install_skill(&claude)? {
        added.push("the forkspace skill");
    }
    if install_hooks(&claude.join("settings.json"))? {
        added.push("the worktree hooks");
    }
    if added.is_empty() {
        println!("Claude Code: already set up");
    } else {
        println!("Claude Code: added {}", added.join(" and "));
    }
    Ok(())
}

/// Write the skill this version ships; whether it changed anything.
fn install_skill(claude: &Path) -> Result<bool> {
    let path = claude.join("skills/forkspace/SKILL.md");
    if fs::read_to_string(&path).is_ok_and(|current| current == super::skill::SKILL) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).io_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&path, super::skill::SKILL).io_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Add forkspace's worktree hooks to `settings`, keeping everything else in it; whether it
/// changed anything. The file is written in place, so a symlinked settings file stays one.
fn install_hooks(settings: &Path) -> Result<bool> {
    let mut current: Value = match fs::read_to_string(settings) {
        Ok(text) if text.trim().is_empty() => Value::Object(Default::default()),
        Ok(text) => serde_json::from_str(&text).map_err(|error| {
            ForkspaceError::InvalidArgument(format!(
                "{} is not valid JSON ({error}); fix it and run `forkspace setup` again",
                settings.display()
            ))
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Value::Object(Default::default())
        }
        Err(error) => return Err(error).io_context(|| format!("read {}", settings.display())),
    };
    let wanted = super::hook::claude_settings();
    let mut changed = false;
    for (event, entries) in wanted["hooks"].as_object().into_iter().flatten() {
        let Some(root) = current.as_object_mut() else {
            return Err(ForkspaceError::InvalidArgument(format!(
                "{} does not hold a JSON object; fix it and run `forkspace setup` again",
                settings.display()
            )));
        };
        let hooks = root
            .entry("hooks")
            .or_insert_with(|| Value::Object(Default::default()));
        let Some(hooks) = hooks.as_object_mut() else {
            continue;
        };
        let list = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(list) = list.as_array_mut() else {
            continue;
        };
        for entry in entries.as_array().into_iter().flatten() {
            let command = &entry["hooks"][0]["command"];
            let present = list.iter().any(|existing| {
                existing["hooks"]
                    .as_array()
                    .is_some_and(|hooks| hooks.iter().any(|hook| hook["command"] == *command))
            });
            if !present {
                list.push(entry.clone());
                changed = true;
            }
        }
    }
    if changed {
        let text = serde_json::to_string_pretty(&current)? + "\n";
        fs::write(settings, text).io_context(|| format!("write {}", settings.display()))?;
    }
    Ok(changed)
}
