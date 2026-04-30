//! Review signals: dependency churn from lockfile diffs, and how many source and test files a
//! change set adds, edits, or deletes.
//!
//! Lockfile parsing is deliberately shallow: each parser extracts package names line by line
//! and returns `None` for anything it does not recognize, so no signal beats a wrong one.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::changes::filter::ChangeClass;
use crate::changes::{ChangeKind, FileChange, OldContent};
use crate::workspace::Workspace;

/// Root-level lockfiles that dependency signals are derived from.
pub const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "bun.lock",
    "bun.lockb",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
];

/// Facts about a change set worth surfacing before it is merged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Signals {
    pub deps_added: usize,
    pub deps_removed: usize,
    pub dep_names_added: Vec<String>,
    pub dep_names_removed: Vec<String>,
    /// File name of the lockfile the dependency counts came from.
    pub dependency_source: Option<String>,
    pub new_files: usize,
    pub edited_files: usize,
    pub tests_touched: usize,
    pub tests_deleted: usize,
}

/// Collect signals for `changes`, reading lockfile endpoints from the base and the view.
///
/// Dependency counts come from the first root lockfile whose both endpoints parse.
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

    let lockfile_changes = changes.iter().filter(|change| {
        change.class == ChangeClass::Dependency
            && LOCKFILES.iter().any(|name| change.path == Path::new(name))
            && change.old_content == OldContent::Available
    });
    for change in lockfile_changes {
        let Some((old, new)) = dependency_endpoints(workspace, change) else {
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

/// One summary line such as `signals: +2 dependencies (a, b) · 3 new / 1 edited`, or `None`
/// when there is nothing to say.
pub fn render(signals: &Signals) -> Option<String> {
    let mut segments = Vec::new();
    if signals.deps_added > 0 {
        segments.push(format!(
            "+{} ({})",
            counted(signals.deps_added, "dependency", "dependencies"),
            first_names(&signals.dep_names_added)
        ));
    }
    if signals.deps_removed > 0 {
        segments.push(format!(
            "-{} ({})",
            counted(signals.deps_removed, "dependency", "dependencies"),
            first_names(&signals.dep_names_removed)
        ));
    }
    if signals.new_files > 0 {
        segments.push(format!(
            "{} new / {} edited",
            signals.new_files, signals.edited_files
        ));
    }
    if signals.tests_deleted > 0 {
        segments.push(format!(
            "{} deleted",
            counted(signals.tests_deleted, "test file", "test files")
        ));
    } else if signals.tests_touched > 0 {
        segments.push(format!(
            "{} touched",
            counted(signals.tests_touched, "test file", "test files")
        ));
    }
    (!segments.is_empty()).then(|| format!("signals: {}", segments.join(" · ")))
}

/// Package names in a lockfile, or `None` if the format is unknown or unparseable.
pub fn parse_lockfile(path: &Path, bytes: &[u8]) -> Option<BTreeSet<String>> {
    match path.file_name()?.to_str()? {
        "package-lock.json" | "bun.lock" => package_lock(bytes),
        "pnpm-lock.yaml" => pnpm_lock(bytes),
        "yarn.lock" => yarn_lock(bytes),
        "Cargo.lock" => cargo_lock(bytes),
        // `bun.lockb` is binary.
        _ => None,
    }
}

/// Heuristic: a `test`/`tests`/`spec`/`__tests__` directory, a `.test.`/`.spec.` file name, or
/// a `_test`/`_spec` suffix or `test_` prefix on the file stem.
pub fn is_test_path(path: &Path) -> bool {
    let in_test_dir = path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("test" | "tests" | "spec" | "__tests__")
        )
    });
    if in_test_dir {
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

/// Package names before (base) and after (view) a lockfile change. An added lockfile starts
/// from nothing and a deleted one ends with nothing.
fn dependency_endpoints(
    workspace: &Workspace,
    change: &FileChange,
) -> Option<(BTreeSet<String>, BTreeSet<String>)> {
    let old = match change.kind {
        ChangeKind::Added => Some(BTreeSet::new()),
        _ => lockfile_names(&workspace.base, &change.path),
    };
    let new = match change.kind {
        ChangeKind::Deleted => Some(BTreeSet::new()),
        _ => lockfile_names(&workspace.view_path, &change.path),
    };
    old.zip(new)
}

fn lockfile_names(root: &Path, relative: &Path) -> Option<BTreeSet<String>> {
    parse_lockfile(relative, &fs::read(root.join(relative)).ok()?)
}

/// npm `package-lock.json` (and text `bun.lock`): v2+ lists `packages` keyed by install path
/// (`node_modules/a/node_modules/b`); v1 lists top-level `dependencies` by name.
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
                .filter(|key| !key.is_empty())
                .map(|key| install_path_name(key).to_string())
                .collect(),
        );
    }
    let dependencies = value.get("dependencies")?.as_object()?;
    Some(dependencies.keys().cloned().collect())
}

/// `node_modules/a/node_modules/@s/b` → `@s/b`.
fn install_path_name(key: &str) -> &str {
    key.rsplit("node_modules/").next().unwrap_or(key)
}

/// `Cargo.lock`: every `name = "…"` line under `[[package]]` tables.
fn cargo_lock(bytes: &[u8]) -> Option<BTreeSet<String>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.contains("[[package]]") {
        return None;
    }
    let names = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("name = \"")?.strip_suffix('"'))
        .map(str::to_string)
        .collect();
    Some(names)
}

/// `pnpm-lock.yaml` with a known `lockfileVersion`: keys of the form `/name@version:` or
/// `/@scope/name@version:`. A scoped key without a version makes the whole file unparseable.
fn pnpm_lock(bytes: &[u8]) -> Option<BTreeSet<String>> {
    const KNOWN_VERSIONS: &[&str] = &["5", "5.4", "6", "7", "8", "9"];
    let text = std::str::from_utf8(bytes).ok()?;
    let version = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("lockfileVersion:"))?
        .trim()
        .trim_matches('\'');
    if !KNOWN_VERSIONS.contains(&version) {
        return None;
    }
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let key = line.trim().trim_matches('\'').trim_end_matches(':');
        let Some(key) = key.strip_prefix('/') else {
            continue;
        };
        let name = pnpm_package_name(key)?;
        if !name.is_empty() {
            names.insert(name.to_string());
        }
    }
    Some(names)
}

/// `name@1.0.0` → `name`; `@scope/name@1.0.0` → `@scope/name`.
fn pnpm_package_name(key: &str) -> Option<&str> {
    if !key.starts_with('@') {
        return key.split('@').next();
    }
    let slash = key.find('/')?;
    let version_at = slash + 1 + key[slash + 1..].rfind('@')?;
    Some(&key[..version_at])
}

/// `yarn.lock`: unindented entry keys with an npm descriptor, `"name@npm:range":`.
fn yarn_lock(bytes: &[u8]) -> Option<BTreeSet<String>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.contains("yarn lockfile") && !text.contains("@npm:") {
        return None;
    }
    let names = text
        .lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| {
            let key = line.trim_matches('"').trim_end_matches(':');
            let name = &key[..key.find("@npm:")?];
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect();
    Some(names)
}

/// Up to five names, then `+N more`.
fn first_names(values: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut shown = values.iter().take(SHOWN).cloned().collect::<Vec<_>>();
    if values.len() > SHOWN {
        shown.push(format!("+{} more", values.len() - SHOWN));
    }
    shown.join(", ")
}

/// `1 dependency`, `2 dependencies`.
fn counted(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}
