//! Package-manager detection and `install` runs after a merge or pull.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::backend::select_backend;
use crate::changes::manifest::ManifestStatus;
use crate::error::{ForkspaceError, Result};
use crate::workspace::Workspace;

/// A JavaScript package manager forkspace knows how to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Bun,
    Pnpm,
    Npm,
    Yarn,
}

/// Lockfiles that identify a package manager, in detection priority order.
const LOCKFILES: &[(&str, PackageManager)] = &[
    ("bun.lockb", PackageManager::Bun),
    ("bun.lock", PackageManager::Bun),
    ("pnpm-lock.yaml", PackageManager::Pnpm),
    ("package-lock.json", PackageManager::Npm),
    ("yarn.lock", PackageManager::Yarn),
];

impl PackageManager {
    fn command(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Pnpm => "pnpm",
            Self::Npm => "npm",
            Self::Yarn => "yarn",
        }
    }

    fn from_command(name: &str) -> Option<Self> {
        match name {
            "bun" => Some(Self::Bun),
            "pnpm" => Some(Self::Pnpm),
            "npm" => Some(Self::Npm),
            "yarn" => Some(Self::Yarn),
            _ => None,
        }
    }
}

/// The package manager a workspace's project uses, if it is a JavaScript project.
///
/// `package.json`'s `packageManager` field wins; otherwise the first lockfile present decides.
pub fn detect_package_manager(workspace: &Workspace) -> Option<PackageManager> {
    detect_in(&project_root(workspace)?)
}

/// The package manager of the JavaScript project in `view`, if it is one.
pub fn detect_in(view: &Path) -> Option<PackageManager> {
    let package_json = view.join("package.json");
    if !package_json.exists() {
        return None;
    }
    let manifest = fs::read(&package_json).ok()?;
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&manifest) {
        if let Some(declared) = value["packageManager"].as_str() {
            let name = declared.split('@').next().unwrap_or(declared);
            return PackageManager::from_command(name);
        }
    }
    LOCKFILES
        .iter()
        .find(|(lockfile, _)| view.join(lockfile).exists())
        .map(|&(_, manager)| manager)
}

/// Files whose change can change what `install` does: package manifests, lockfiles, and
/// package-manager configuration. A transfer that touches none of them leaves the installed
/// dependencies as they were, so the install step is skipped.
const DEPENDENCY_FILES: &[&str] = &[
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "bun.lock",
    "bun.lockb",
    "bunfig.toml",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pnpmfile.cjs",
];

/// Whether any of `paths` is a file that can change the result of a package install.
pub fn touches_dependencies<P: AsRef<Path>>(paths: impl IntoIterator<Item = P>) -> bool {
    paths.into_iter().any(|path| {
        path.as_ref()
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| DEPENDENCY_FILES.contains(&name))
    })
}

/// Run `<manager> install` in `directory`.
pub(crate) fn run_install(directory: &Path, manager: PackageManager) -> Result {
    let command = manager.command();
    let status = Command::new(command)
        .arg("install")
        .current_dir(directory)
        .status()
        .map_err(|error| {
            ForkspaceError::PackageManagerFailed(format!(
                "failed to run '{command} install': {error}. Is {command} installed and on your PATH?"
            ))
        })?;
    if !status.success() {
        return Err(ForkspaceError::PackageManagerFailed(format!(
            "'{command} install' exited with status {status}"
        )));
    }
    Ok(())
}

/// Where the workspace's own files are visible: the upper layer for an untracked overlay,
/// otherwise the backend's view.
fn project_root(workspace: &Workspace) -> Option<PathBuf> {
    if workspace.manifest_status == ManifestStatus::Absent {
        return workspace.upper_path().ok().map(Path::to_path_buf);
    }
    Some(
        select_backend(Some(workspace.backend_kind))
            .ok()?
            .view_path(workspace),
    )
}

#[cfg(test)]
mod tests {
    use super::touches_dependencies;

    #[test]
    fn only_manifests_lockfiles_and_settings_touch_dependencies() {
        assert!(!touches_dependencies([
            "app/page.tsx",
            "src/package.ts",
            "README.md"
        ]));
        assert!(touches_dependencies(["app/page.tsx", "package.json"]));
        assert!(touches_dependencies(["packages/ui/package.json"]));
        assert!(touches_dependencies(["pnpm-lock.yaml"]));
        assert!(touches_dependencies([".npmrc"]));
        assert!(!touches_dependencies(Vec::<&str>::new()));
    }
}
