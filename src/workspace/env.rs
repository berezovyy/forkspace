//! What processes started inside a workspace see, and what they inherited that still points
//! outside it.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::backend::select_backend;
use crate::error::Result;
use crate::state::State;
use crate::workspace::Workspace;

/// Environment for any process forkspace starts in `workspace` (`enter` and `run`).
///
/// Besides the `FORKSPACE_*` variables, `COMPOSE_PROJECT_NAME` is set (unless the user set it)
/// so Docker Compose gives each workspace its own containers, networks, and volumes instead
/// of deriving one shared project name from the view directory's basename.
pub fn process_env(state: &State, workspace: &Workspace) -> Result<Vec<(String, String)>> {
    let mut env = vec![
        ("FORKSPACE_WORKSPACE".to_string(), workspace.name.clone()),
        (
            "FORKSPACE_BASE".to_string(),
            workspace.base.display().to_string(),
        ),
    ];
    env.extend(select_backend(Some(workspace.backend_kind))?.shell_env(workspace));
    if std::env::var_os("COMPOSE_PROJECT_NAME").is_none() {
        env.push((
            "COMPOSE_PROJECT_NAME".to_string(),
            compose_project_name(state, workspace),
        ));
    }
    Ok(env)
}

/// `<root project directory name>-<workspace name>`, reduced to what Compose accepts:
/// lowercase letters, digits, `-` and `_`, starting with a letter or digit.
fn compose_project_name(state: &State, workspace: &Workspace) -> String {
    let chain = state.parent_chain(&workspace.name);
    let root_base = chain.first().map_or(&workspace.base, |root| &root.base);
    let project = root_base
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let raw = format!("{project}-{}", workspace.name).to_lowercase();
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    if trimmed.is_empty() {
        "forkspace".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A Python virtual environment in the view whose scripts still run another copy of itself.
pub struct InheritedVenv {
    /// The environment's directory, relative to the view.
    pub path: PathBuf,
    /// Where its scripts and `activate` point instead.
    pub points_to: PathBuf,
}

/// Python virtual environments the fork copied from the base. Their `activate` script and
/// script shebangs hold absolute paths, so `.venv/bin/pip install` in the workspace runs the
/// base's interpreter and installs into the base. Looks at the view's top two levels.
pub fn inherited_venvs(workspace: &Workspace) -> Vec<InheritedVenv> {
    let view = &workspace.view_path;
    let mut found = Vec::new();
    for directory in candidate_dirs(view) {
        if !directory.join("pyvenv.cfg").is_file() {
            continue;
        }
        if let Some(points_to) = foreign_venv_path(&directory) {
            if let Ok(path) = directory.strip_prefix(view) {
                found.push(InheritedVenv {
                    path: path.to_path_buf(),
                    points_to,
                });
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Directories at depth 1 and 2 under `view`, skipping trees that never hold a project venv.
fn candidate_dirs(view: &Path) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[".git", "node_modules", "target"];
    let subdirs = |dir: &Path| -> Vec<PathBuf> {
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .filter(|entry| !SKIP.contains(&entry.file_name().to_string_lossy().as_ref()))
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    let top = subdirs(view);
    let nested: Vec<PathBuf> = top.iter().flat_map(|dir| subdirs(dir)).collect();
    top.into_iter().chain(nested).collect()
}

/// The environment directory `venv`'s `activate` script or script shebangs name, if it is not
/// `venv` itself.
fn foreign_venv_path(venv: &Path) -> Option<PathBuf> {
    let bin = venv.join("bin");
    if let Some(recorded) = activate_virtual_env(&bin.join("activate")) {
        if !same_dir(&recorded, venv) {
            return Some(recorded);
        }
    }
    let entries = fs::read_dir(&bin).ok()?;
    entries.flatten().find_map(|entry| {
        let interpreter = shebang_interpreter(&entry.path())?;
        // `<env>/bin/python…`; `#!/usr/bin/env python3` and system interpreters are skipped
        // because their grandparent directory is not a virtual environment.
        let env_dir = interpreter.parent()?.parent()?;
        let is_venv_python = interpreter
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("python"))
            && env_dir.join("pyvenv.cfg").is_file();
        (is_venv_python && !same_dir(env_dir, venv)).then(|| env_dir.to_path_buf())
    })
}

/// Whether two paths name the same directory, comparing canonical forms when they exist.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The `VIRTUAL_ENV=` value an `activate` script assigns.
fn activate_virtual_env(activate: &Path) -> Option<PathBuf> {
    let script = fs::read_to_string(activate).ok()?;
    script.lines().find_map(|line| {
        let value = line.trim().strip_prefix("VIRTUAL_ENV=")?;
        let value = value.trim_matches(|c| c == '\'' || c == '"');
        value.starts_with('/').then(|| PathBuf::from(value))
    })
}

/// The absolute interpreter path in a `#!` line, if `path` starts with one.
fn shebang_interpreter(path: &Path) -> Option<PathBuf> {
    let mut head = [0u8; 512];
    let read = fs::File::open(path).ok()?.read(&mut head).ok()?;
    let line = head[..read].split(|byte| *byte == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.strip_prefix("#!")?;
    let interpreter = line.split_whitespace().next()?;
    interpreter
        .starts_with('/')
        .then(|| PathBuf::from(interpreter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_names_are_sanitized() {
        let state = State::default();
        let mut workspace = Workspace::new("Agent.A", PathBuf::from("/src/My App"), None);
        assert_eq!(compose_project_name(&state, &workspace), "my-app-agent-a");
        workspace.base = PathBuf::from("/");
        assert_eq!(compose_project_name(&state, &workspace), "agent-a");
    }
}
