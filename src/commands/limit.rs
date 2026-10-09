//! `forkspace limit`: show or change a workspace's memory and CPU limits.

use crate::cli::LimitArgs;
use crate::error::{ForkspaceError, Result};
use crate::process::systemd;
use crate::state::State;
use crate::workspace::Limits;

pub fn exec(args: LimitArgs) -> Result {
    let name = args.name;
    let limits = State::with_state(|state| {
        let workspace = state.require_workspace_mut(&name)?;
        let mut properties = Vec::new();
        if let Some(memory) = args.memory.as_deref() {
            workspace.limits.memory = parse_memory(memory)?;
            let value = workspace.limits.memory.as_deref().unwrap_or("infinity");
            properties.push(format!("MemoryHigh={value}"));
        }
        if let Some(cpu) = args.cpu.as_deref() {
            workspace.limits.cpu = parse_cpu(cpu)?;
            properties.push(format!(
                "CPUQuota={}",
                workspace.limits.cpu.as_deref().unwrap_or("")
            ));
        }
        // Running processes get the change now; later ones get it when they start.
        systemd::set_properties(&name, &properties);
        Ok(workspace.limits.clone())
    })?;
    println!(
        "'{name}': memory {}, cpu {}",
        limits.memory.as_deref().unwrap_or("unlimited"),
        limits.cpu.as_deref().unwrap_or("unlimited")
    );
    warn_without_systemd(&limits);
    Ok(())
}

/// Limits from `new --memory/--cpu`, validated before anything is created.
pub(crate) fn from_flags(memory: Option<&str>, cpu: Option<&str>) -> Result<Limits> {
    Ok(Limits {
        memory: memory.map(parse_memory).transpose()?.flatten(),
        cpu: cpu.map(parse_cpu).transpose()?.flatten(),
    })
}

pub(crate) fn warn_without_systemd(limits: &Limits) {
    if !limits.is_empty() && !systemd::available() {
        eprintln!(
            "warning: memory and CPU limits need a systemd user session (Linux); processes in this workspace run without them here"
        );
    }
}

/// `4G`, `512M`, `75%`, or `none` (no limit).
fn parse_memory(value: &str) -> Result<Option<String>> {
    if value == "none" {
        return Ok(None);
    }
    let digits = value.trim_end_matches(['K', 'M', 'G', 'T', '%']);
    let suffix = &value[digits.len()..];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) || suffix.len() > 1 {
        return Err(ForkspaceError::InvalidArgument(format!(
            "invalid memory limit '{value}'; use bytes with an optional K, M, G, or T suffix (4G), a percentage of RAM (25%), or none"
        )));
    }
    Ok(Some(value.to_string()))
}

/// `50%`, `200%`, or `none` (no limit).
fn parse_cpu(value: &str) -> Result<Option<String>> {
    if value == "none" {
        return Ok(None);
    }
    let valid = value.strip_suffix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    if !valid {
        return Err(ForkspaceError::InvalidArgument(format!(
            "invalid CPU limit '{value}'; use a percentage of one CPU (50%, 200%) or none"
        )));
    }
    Ok(Some(value.to_string()))
}
