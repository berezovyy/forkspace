//! Parallel walks for the scans that must visit a whole tree, such as manifest builds and the
//! frozen-copy freshness check. Directories are read on several threads at once, which is
//! what makes metadata-heavy walks of large `node_modules` trees fast.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ignore::{WalkBuilder, WalkState};

use crate::error::{ForkspaceError, Result};

/// What a [`parallel_with`] visitor wants done with an entry.
pub enum Step<T> {
    /// Keep the item (if any) and, for a directory, descend into it.
    Descend(Option<T>),
    /// Keep the item (if any) but do not descend into this directory.
    Prune(Option<T>),
}

/// Visit every entry under `root` (not `root` itself) without following symlinks and without
/// honouring ignore files. `visit` receives the path relative to `root`, the full path, and
/// the entry's `lstat` metadata, and may keep an item; the first error stops the walk and is
/// returned. Kept items come back sorted by relative path, so output does not depend on
/// thread timing.
pub fn parallel<T, F>(root: &Path, visit: F) -> Result<Vec<(PathBuf, T)>>
where
    T: Send,
    F: Fn(&Path, &Path, &fs::Metadata) -> Result<Option<T>> + Sync,
{
    parallel_with(root, |relative, path, metadata| {
        visit(relative, path, metadata).map(Step::Descend)
    })
}

/// [`parallel`] whose visitor can also prune directories.
pub fn parallel_with<T, F>(root: &Path, visit: F) -> Result<Vec<(PathBuf, T)>>
where
    T: Send,
    F: Fn(&Path, &Path, &fs::Metadata) -> Result<Step<T>> + Sync,
{
    let kept = Mutex::new(Vec::new());
    let failure = Mutex::new(None);
    let walker = WalkBuilder::new(root)
        .standard_filters(false)
        .follow_links(false)
        .threads(threads())
        .build_parallel();
    walker.run(|| {
        Box::new(|entry| {
            let outcome = entry
                .map_err(|error| walk_error(root, error))
                .and_then(|entry| {
                    if entry.depth() == 0 {
                        return Ok((None, WalkState::Continue));
                    }
                    let path = entry.path();
                    let metadata =
                        fs::symlink_metadata(path).map_err(|source| ForkspaceError::Io {
                            context: format!("inspect {}", path.display()),
                            source,
                        })?;
                    let relative = path.strip_prefix(root).map_err(|_| {
                        ForkspaceError::StateFailed(format!(
                            "{} escaped the walk of {}",
                            path.display(),
                            root.display()
                        ))
                    })?;
                    let (item, state) = match visit(relative, path, &metadata)? {
                        Step::Descend(item) => (item, WalkState::Continue),
                        Step::Prune(item) => (item, WalkState::Skip),
                    };
                    Ok((item.map(|item| (relative.to_path_buf(), item)), state))
                });
            match outcome {
                Ok((Some(item), state)) => {
                    kept.lock().expect("walk results lock").push(item);
                    state
                }
                Ok((None, state)) => state,
                Err(error) => {
                    failure
                        .lock()
                        .expect("walk failure lock")
                        .get_or_insert(error);
                    WalkState::Quit
                }
            }
        })
    });
    if let Some(error) = failure.into_inner().expect("walk failure lock") {
        return Err(error);
    }
    let mut kept = kept.into_inner().expect("walk results lock");
    kept.sort_by(|a, b| {
        a.0.as_os_str()
            .as_encoded_bytes()
            .cmp(b.0.as_os_str().as_encoded_bytes())
    });
    Ok(kept)
}

fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().min(8))
        .unwrap_or(4)
}

fn walk_error(root: &Path, error: ignore::Error) -> ForkspaceError {
    let source = match error.io_error() {
        Some(io) => std::io::Error::new(io.kind(), error.to_string()),
        None => std::io::Error::other(error.to_string()),
    };
    ForkspaceError::Io {
        context: format!("failed to walk {}", root.display()),
        source,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn visits_every_entry_once_in_path_order() {
        let root = tempfile::tempdir().unwrap();
        for directory in ["a/b", "c", ".hidden", "node_modules/x"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        std::fs::write(root.path().join(".gitignore"), "c\n").unwrap();
        std::fs::write(root.path().join("a/b/file"), "x").unwrap();
        std::os::unix::fs::symlink("missing", root.path().join("link")).unwrap();
        let seen = super::parallel(root.path(), |_, _, _| Ok(Some(()))).unwrap();
        let paths: Vec<String> = seen
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect();
        assert_eq!(
            paths,
            [
                ".gitignore",
                ".hidden",
                "a",
                "a/b",
                "a/b/file",
                "c",
                "link",
                "node_modules",
                "node_modules/x"
            ]
        );
    }
}
