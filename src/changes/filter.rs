//! Which changes are worth showing: path classification and ignore rules.
//!
//! Every change gets a [`ChangeClass`]. By default only source and dependency changes are
//! shown, and source paths are further filtered by `.gitignore`, `.forkspaceignore`, and a
//! built-in `node_modules/` rule.

use std::ffi::OsStr;
use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::changes::FileChange;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ChangeClass {
    /// Project files; subject to ignore rules.
    Source,
    /// Manifests and lockfiles (`package.json`, `Cargo.lock`, ...); always shown.
    Dependency,
    /// Installed or OS-generated files (`node_modules/`, `.DS_Store`); never shown.
    Runtime,
    /// Version-control internals under `.git/`; never shown.
    Internal,
}

/// Ignore rules of a base directory.
pub struct ChangeFilter {
    gitignore: Gitignore,
}

impl ChangeFilter {
    pub fn build(base: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(base);
        let _ = builder.add_line(None, "node_modules/");
        for name in [".gitignore", ".forkspaceignore"] {
            let path = base.join(name);
            if path.exists() {
                let _ = builder.add(&path);
            }
        }
        let gitignore = builder.build().unwrap_or_else(|_| {
            GitignoreBuilder::new(base)
                .build()
                .expect("an empty gitignore builds")
        });
        Self { gitignore }
    }

    pub fn is_ignored(&self, rel_path: &Path, is_dir: bool) -> bool {
        self.gitignore
            .matched_path_or_any_parents(rel_path, is_dir)
            .is_ignore()
    }

    pub fn class(path: &Path) -> ChangeClass {
        let has_component = |name: &str| {
            path.components()
                .any(|component| component.as_os_str() == name)
        };
        if has_component(".git") {
            return ChangeClass::Internal;
        }
        if has_component("node_modules") || path.file_name().is_some_and(|name| name == ".DS_Store")
        {
            return ChangeClass::Runtime;
        }
        if path.file_name().is_some_and(is_dependency_name) {
            return ChangeClass::Dependency;
        }
        ChangeClass::Source
    }

    pub fn is_visible(&self, rel_path: &Path, is_dir: bool) -> bool {
        match Self::class(rel_path) {
            ChangeClass::Dependency => true,
            ChangeClass::Runtime | ChangeClass::Internal => false,
            ChangeClass::Source => !self.is_ignored(rel_path, is_dir),
        }
    }

    pub fn filter_changes(&self, changes: Vec<FileChange>) -> Vec<FileChange> {
        changes
            .into_iter()
            .filter(|change| self.is_visible(&change.path, false))
            .collect()
    }
}

fn is_dependency_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(
        name,
        "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "Cargo.toml"
            | "Cargo.lock"
            | "go.mod"
            | "go.sum"
            | "pyproject.toml"
            | "uv.lock"
    ) || name.starts_with("bun.lock")
        || (name.starts_with("requirements") && name.ends_with(".txt"))
}
