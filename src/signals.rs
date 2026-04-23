use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::diff::{ChangeKind, FileChange, OldContent};
use crate::filter::ChangeClass;
use crate::workspace::Workspace;

pub const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "bun.lock",
    "bun.lockb",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Signals {
    pub deps_added: usize,
    pub deps_removed: usize,
    pub dep_names_added: Vec<String>,
    pub dep_names_removed: Vec<String>,
    pub dependency_source: Option<String>,
    pub new_files: usize,
    pub edited_files: usize,
    pub tests_touched: usize,
    pub tests_deleted: usize,
}

fn package_lock(bytes: &[u8]) -> Option<BTreeSet<String>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let version = value
        .get("lockfileVersion")
        .and_then(|value| value.as_u64());
    if version.is_some_and(|version| version >= 2) {
        let packages = value.get("packages")?.as_object()?;
        return Some(
            packages
                .keys()
                .filter(|name| !name.is_empty())
                .filter_map(|name| name.rsplit("node_modules/").next())
                .map(str::to_string)
                .collect(),
        );
    }
    Some(
        value
            .get("dependencies")?
            .as_object()?
            .keys()
            .cloned()
            .collect(),
    )
}

fn cargo_lock(bytes: &[u8]) -> Option<BTreeSet<String>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.contains("[[package]]") {
        return None;
    }
    Some(
        text.lines()
            .filter_map(|line| line.trim().strip_prefix("name = \"")?.strip_suffix('"'))
            .map(str::to_string)
            .collect(),
    )
}

fn pnpm_lock(bytes: &[u8]) -> Option<BTreeSet<String>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let version = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("lockfileVersion:").map(str::trim))?;
    if !matches!(
        version.trim_matches('\''),
        "5" | "5.4" | "6" | "7" | "8" | "9"
    ) {
        return None;
    }
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let key = line.trim().trim_matches('\'').trim_end_matches(':');
        let Some(key) = key.strip_prefix('/') else {
            continue;
        };
        let name = if key.starts_with('@') {
            let slash = key.find('/')?;
            let version = key[slash + 1..].rfind('@')? + slash + 1;
            &key[..version]
        } else {
            key.split('@').next()?
        };
        if !name.is_empty() {
            names.insert(name.to_string());
        }
    }
    Some(names)
}

fn yarn_lock(bytes: &[u8]) -> Option<BTreeSet<String>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.contains("yarn lockfile") && !text.contains("@npm:") {
        return None;
    }
    let mut names = BTreeSet::new();
    for line in text
        .lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
    {
        let key = line.trim_matches('"').trim_end_matches(':');
        let Some(index) = key.find("@npm:") else {
            continue;
        };
        let name = &key[..index];
        if !name.is_empty() {
            names.insert(name.to_string());
        }
    }
    Some(names)
}

pub fn parse_lockfile(path: &Path, bytes: &[u8]) -> Option<BTreeSet<String>> {
    match path.file_name()?.to_str()? {
        "package-lock.json" | "bun.lock" => package_lock(bytes),
        "pnpm-lock.yaml" => pnpm_lock(bytes),
        "yarn.lock" => yarn_lock(bytes),
        "Cargo.lock" => cargo_lock(bytes),
        "bun.lockb" => None,
        _ => None,
    }
}

pub fn is_test_path(path: &Path) -> bool {
    if path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| matches!(value, "test" | "tests" | "spec" | "__tests__"))
    }) {
        return true;
    }
    let Some(file) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    if file.contains(".test.") || file.contains(".spec.") {
        return true;
    }
    path.file_stem()
        .and_then(|value| value.to_str())
        .is_some_and(|stem| {
            stem.ends_with("_test") || stem.ends_with("_spec") || stem.starts_with("test_")
        })
}

fn endpoint(root: &Path, relative: &Path) -> Option<Vec<u8>> {
    fs::read(root.join(relative)).ok()
}

pub fn collect(workspace: &Workspace, changes: &[FileChange]) -> Signals {
    let mut signals = Signals::default();
    for change in changes
        .iter()
        .filter(|change| change.class == ChangeClass::Source)
    {
        match change.kind {
            ChangeKind::Added => signals.new_files += 1,
            ChangeKind::Modified => signals.edited_files += 1,
            ChangeKind::Deleted => {}
        }
        if is_test_path(&change.path) {
            signals.tests_touched += 1;
            if change.kind == ChangeKind::Deleted {
                signals.tests_deleted += 1;
            }
        }
    }

    for change in changes.iter().filter(|change| {
        change.class == ChangeClass::Dependency
            && LOCKFILES
                .iter()
                .any(|name| change.path == PathBuf::from(name))
            && change.old_content == OldContent::Available
    }) {
        let old = match change.kind {
            ChangeKind::Added => Some(BTreeSet::new()),
            _ => endpoint(&workspace.base, &change.path)
                .as_deref()
                .and_then(|bytes| parse_lockfile(&change.path, bytes)),
        };
        let new = match change.kind {
            ChangeKind::Deleted => Some(BTreeSet::new()),
            _ => endpoint(&workspace.view_path, &change.path)
                .as_deref()
                .and_then(|bytes| parse_lockfile(&change.path, bytes)),
        };
        let (Some(old), Some(new)) = (old, new) else {
            continue;
        };
        signals.dep_names_added = new.difference(&old).cloned().collect();
        signals.dep_names_removed = old.difference(&new).cloned().collect();
        signals.deps_added = signals.dep_names_added.len();
        signals.deps_removed = signals.dep_names_removed.len();
        signals.dependency_source = change
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        break;
    }
    signals
}

fn names(values: &[String]) -> String {
    let mut shown = values.iter().take(5).cloned().collect::<Vec<_>>();
    if values.len() > 5 {
        shown.push(format!("+{} more", values.len() - 5));
    }
    shown.join(", ")
}

pub fn render(signals: &Signals) -> Option<String> {
    let mut segments = Vec::new();
    if signals.deps_added > 0 {
        segments.push(format!(
            "+{} dependenc{} ({})",
            signals.deps_added,
            if signals.deps_added == 1 { "y" } else { "ies" },
            names(&signals.dep_names_added)
        ));
    }
    if signals.deps_removed > 0 {
        segments.push(format!(
            "-{} dependenc{} ({})",
            signals.deps_removed,
            if signals.deps_removed == 1 {
                "y"
            } else {
                "ies"
            },
            names(&signals.dep_names_removed)
        ));
    }
    if signals.new_files > 0 {
        segments.push(format!(
            "{} new / {} edited",
            signals.new_files, signals.edited_files
        ));
    }
    if signals.tests_deleted > 0 {
        segments.push(format!("{} test files deleted", signals.tests_deleted));
    } else if signals.tests_touched > 0 {
        segments.push(format!("{} test files touched", signals.tests_touched));
    }
    (!segments.is_empty()).then(|| format!("signals: {}", segments.join(" · ")))
}
