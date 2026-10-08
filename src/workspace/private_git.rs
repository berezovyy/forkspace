//! A private Git repository for workspaces forked from a linked worktree or a submodule.
//!
//! There `.git` is a file pointing at repository data outside the project, so a plain fork
//! shares `HEAD`, the index, and every ref with the project: a commit in the workspace moves
//! the user's own branch. The fork replaces that pointer with a standalone `.git` directory
//! holding copies of `HEAD`, the index, config, refs, and logs, and borrowing the object store
//! through `objects/info/alternates` (Git's own mechanism; nothing in the original repository
//! changes). Commits made in the workspace stay there until `forkspace land` fetches them.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::git;
use crate::util::{IoContext, WalkDirExt};

/// Give `view` its own repository when `base/.git` points outside `base`. Returns whether it
/// did.
pub(crate) fn privatize(base: &Path, view: &Path) -> Result<bool> {
    let Some(git_dir) = external_git_dir(base) else {
        return Ok(false);
    };
    let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(relative) => git_dir.join(relative.trim()),
        Err(_) => git_dir.clone(),
    };
    let common_dir = common_dir
        .canonicalize()
        .io_context(|| format!("resolve Git common directory {}", common_dir.display()))?;

    let staging = view.join(".git.forkspace-private");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir(&staging).io_context(|| format!("create {}", staging.display()))?;
    let built = build(&git_dir, &common_dir, &staging);
    if let Err(error) = built {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    let pointer = view.join(".git");
    fs::remove_file(&pointer).io_context(|| format!("remove {}", pointer.display()))?;
    fs::rename(&staging, &pointer).io_context(|| format!("install {}", pointer.display()))?;
    Ok(true)
}

/// The repository `base/.git` points to, when that is a file naming a directory outside
/// `base`. A pointer into the project itself is copied with the project and stays private.
fn external_git_dir(base: &Path) -> Option<PathBuf> {
    let pointer = base.join(".git");
    if !fs::symlink_metadata(&pointer).ok()?.is_file() {
        return None;
    }
    let contents = fs::read_to_string(&pointer).ok()?;
    let target = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?;
    let git_dir = base.join(target.trim()).canonicalize().ok()?;
    let base = base.canonicalize().ok()?;
    (!git_dir.starts_with(&base)).then_some(git_dir)
}

/// Fill `staging` with a standalone copy of the worktree's view of the repository.
fn build(git_dir: &Path, common_dir: &Path, staging: &Path) -> Result {
    copy_file(&git_dir.join("HEAD"), &staging.join("HEAD"))?;
    copy_if_present(&git_dir.join("index"), &staging.join("index"))?;
    for name in ["config", "packed-refs", "shallow"] {
        copy_if_present(&common_dir.join(name), &staging.join(name))?;
    }
    copy_if_present(
        &git_dir.join("config.worktree"),
        &staging.join("config.worktree"),
    )?;
    // Shared directories first, then the worktree's own entries over them.
    for name in ["refs", "logs", "info", "hooks"] {
        copy_tree(&common_dir.join(name), &staging.join(name))?;
    }
    for name in ["refs", "logs", "info"] {
        if git_dir != common_dir {
            copy_tree(&git_dir.join(name), &staging.join(name))?;
        }
    }
    fs::create_dir_all(staging.join("refs/heads"))
        .and_then(|()| fs::create_dir_all(staging.join("objects/pack")))
        .and_then(|()| fs::create_dir_all(staging.join("objects/info")))
        .io_context(|| format!("create repository layout in {}", staging.display()))?;
    let alternates = format!("{}\n", common_dir.join("objects").display());
    fs::write(staging.join("objects/info/alternates"), alternates)
        .io_context(|| format!("write alternates in {}", staging.display()))?;

    // The copied config may tie the repository to another checkout (submodules set
    // core.worktree) or declare it bare (a worktree of a bare repository).
    for file in ["config", "config.worktree"] {
        let path = staging.join(file);
        if path.exists() {
            let path = path.to_string_lossy();
            let _ = git::capture_output(
                staging,
                &["config", "--file", &path, "--unset-all", "core.worktree"],
            );
        }
    }
    let config = staging.join("config").to_string_lossy().into_owned();
    git::run_git(
        staging,
        &["config", "--file", &config, "core.bare", "false"],
    )
}

fn copy_file(source: &Path, target: &Path) -> Result {
    fs::copy(source, target)
        .map(drop)
        .io_context(|| format!("copy {} to {}", source.display(), target.display()))
}

fn copy_if_present(source: &Path, target: &Path) -> Result {
    if source.is_file() {
        copy_file(source, target)?;
    }
    Ok(())
}

/// Copy the directory tree `source` over `target` (files replace files); absent is fine.
fn copy_tree(source: &Path, target: &Path) -> Result {
    if !source.is_dir() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry = entry.walk_context(source)?;
        let Ok(relative) = entry.path().strip_prefix(source) else {
            continue;
        };
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination)
                .io_context(|| format!("create {}", destination.display()))?;
        } else if entry.file_type().is_file() {
            copy_file(entry.path(), &destination)?;
        }
    }
    Ok(())
}
