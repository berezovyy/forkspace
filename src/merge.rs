use std::fs;
use std::path::Path;
use std::process::Command;

use crate::backend::select_backend;
use crate::diff::{self, ChangeKind, FileChange};
use crate::error::{ForkspaceError, Result};
use crate::workspace::Workspace;

pub struct MergeResult {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub skipped: usize,
}

pub enum PackageManager {
    Bun,
    Pnpm,
    Npm,
    Yarn,
}

impl PackageManager {
    fn command(&self) -> &str {
        match self {
            PackageManager::Bun => "bun",
            PackageManager::Pnpm => "pnpm",
            PackageManager::Npm => "npm",
            PackageManager::Yarn => "yarn",
        }
    }
}

pub fn merge_workspace(workspace: &Workspace) -> Result<MergeResult> {
    let plan = crate::merge_plan::build(workspace, false)?;
    let added = plan
        .changes
        .iter()
        .filter(|change| matches!(change.kind, ChangeKind::Added))
        .count();
    let modified = plan
        .changes
        .iter()
        .filter(|change| matches!(change.kind, ChangeKind::Modified))
        .count();
    let deleted = plan
        .changes
        .iter()
        .filter(|change| matches!(change.kind, ChangeKind::Deleted))
        .count();
    if plan.changes.is_empty() {
        return Ok(MergeResult {
            added,
            modified,
            deleted,
            skipped: plan.skipped,
        });
    }
    if workspace.manifest_status == crate::manifest::ManifestStatus::Absent {
        crate::merge_plan::execute_without_journal(&plan)?;
    } else {
        let journal = crate::merge_plan::execute(&plan)?;
        crate::merge_plan::finish(&journal)?;
    }

    Ok(MergeResult {
        added,
        modified,
        deleted,
        skipped: plan.skipped,
    })
}

pub fn detect_package_manager(workspace: &Workspace) -> Option<PackageManager> {
    let view = if workspace.manifest_status == crate::manifest::ManifestStatus::Absent {
        workspace.upper_path().ok()?.to_path_buf()
    } else {
        select_backend(Some(workspace.backend_kind))
            .ok()?
            .view_path(workspace)
    };

    let package_json = view.join("package.json");
    if !package_json.exists() {
        return None;
    }

    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&fs::read(&package_json).ok()?) {
        if let Some(manager) = value["packageManager"].as_str() {
            let name = manager.split('@').next().unwrap_or(manager);
            return match name {
                "bun" => Some(PackageManager::Bun),
                "pnpm" => Some(PackageManager::Pnpm),
                "npm" => Some(PackageManager::Npm),
                "yarn" => Some(PackageManager::Yarn),
                _ => None,
            };
        }
    }

    if view.join("bun.lockb").exists() || view.join("bun.lock").exists() {
        return Some(PackageManager::Bun);
    }
    if view.join("pnpm-lock.yaml").exists() {
        return Some(PackageManager::Pnpm);
    }
    if view.join("package-lock.json").exists() {
        return Some(PackageManager::Npm);
    }
    if view.join("yarn.lock").exists() {
        return Some(PackageManager::Yarn);
    }

    None
}

pub(crate) fn run_install(base: &Path, pm: &PackageManager) -> Result {
    let cmd = pm.command();
    let status = Command::new(cmd)
        .arg("install")
        .current_dir(base)
        .status()
        .map_err(|e| {
            ForkspaceError::PackageManagerFailed(format!(
                "failed to run '{cmd} install': {e}. Is {cmd} installed and on your PATH?"
            ))
        })?;

    if !status.success() {
        return Err(ForkspaceError::PackageManagerFailed(format!(
            "'{cmd} install' exited with status {status}"
        )));
    }

    Ok(())
}

pub fn generate_patch(workspace: &Workspace, changes: &[FileChange]) -> Result<String> {
    let mut patch = String::new();
    for change in changes {
        let diff_text = diff::generate_unified_diff(workspace, change);
        patch.push_str(&diff_text);
        if !diff_text.ends_with('\n') {
            patch.push('\n');
        }
    }
    Ok(patch)
}

pub fn workspace_head_changed(workspace: &Workspace) -> Result<bool> {
    Ok(select_backend(Some(workspace.backend_kind))?
        .changes(workspace, true, false)?
        .iter()
        .any(|change| change.path == Path::new(".git/HEAD")))
}

pub fn git_commit(base: &Path, message: &str) -> Result {
    if !crate::git::is_repo(base) {
        return Err(ForkspaceError::GitFailed(
            "base directory is not a git repository".to_string(),
        ));
    }

    crate::git::run_git(base, &["add", "-A"])?;
    crate::git::run_git(base, &["commit", "-m", message])?;

    Ok(())
}
