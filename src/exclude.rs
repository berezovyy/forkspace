//! Paths a fork leaves out of a workspace, and that never move between a workspace and its
//! base.
//!
//! - Git worktrees nested inside the project, such as Claude Code's `.claude/worktrees/*`. Their
//!   `.git` file names the project's repository by absolute path, so a copy would commit to
//!   the user's real branches from inside the workspace; they are other checkouts, not part of
//!   this one.
//! - `.claude/worktrees/`, where Claude Code keeps its agents' worktrees and their leftovers.
//! - Whatever the project's `.forkspaceexclude` lists, in `.gitignore` syntax (`!` patterns
//!   can take back the built-in rule above; nested worktrees always stay out).

use std::fs;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// The project file that lists further paths to leave out of workspaces.
pub const FILE: &str = ".forkspaceexclude";

/// Left out of every workspace unless `.forkspaceexclude` takes it back.
pub const BUILT_IN: &[&str] = &[".claude/worktrees/"];

/// The exclusions of one project directory.
pub struct Exclusions {
    matcher: Gitignore,
    /// Linked Git worktrees inside the project, relative to it.
    nested: Vec<PathBuf>,
    /// Whether the project has its own `.forkspaceexclude`.
    custom: bool,
}

impl Exclusions {
    /// The exclusions of the project at `root`.
    pub fn of(root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        for line in BUILT_IN {
            let _ = builder.add_line(None, line);
        }
        let file = root.join(FILE);
        let custom = fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file());
        if custom {
            let _ = builder.add(&file);
        }
        let matcher = builder.build().unwrap_or_else(|_| Gitignore::empty());
        Self {
            matcher,
            nested: nested_worktrees(root),
            custom,
        }
    }

    /// Whether `relative` (a path inside the project) or a directory above it is left out.
    pub fn is_excluded(&self, relative: &Path, is_dir: bool) -> bool {
        if relative.as_os_str().is_empty() || relative.is_absolute() {
            return false;
        }
        // A change list does not always say whether an entry is a directory: a path that
        // would be excluded as one is excluded either way.
        let matched = |is_dir| {
            self.matcher
                .matched_path_or_any_parents(relative, is_dir)
                .is_ignore()
        };
        self.nested
            .iter()
            .any(|nested| relative.starts_with(nested))
            || matched(is_dir)
            || (!is_dir && matched(true))
    }

    /// The nested linked worktrees, relative to the project.
    pub fn nested_worktrees(&self) -> &[PathBuf] {
        &self.nested
    }

    /// The topmost excluded entries that exist under `root`, relative to it, in path order.
    /// Without a `.forkspaceexclude` only the fixed paths are checked; with one, the project is
    /// walked (not below an excluded directory) to find what its patterns match.
    pub fn present(&self, root: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = if self.custom {
            crate::util::walk::parallel_with(root, |relative, _, entry| {
                Ok(if self.is_excluded(relative, entry.is_dir()) {
                    crate::util::walk::Step::Prune(Some(()))
                } else {
                    crate::util::walk::Step::Descend(None)
                })
            })
            .map(|walked| walked.into_iter().map(|(path, ())| path).collect())
            .unwrap_or_default()
        } else {
            BUILT_IN
                .iter()
                .map(|line| PathBuf::from(line.trim_end_matches('/')))
                .chain(self.nested.iter().cloned())
                .filter(|relative| fs::symlink_metadata(root.join(relative)).is_ok())
                .filter(|relative| self.is_excluded(relative, true))
                .collect()
        };
        found.sort();
        // Keep only the topmost: a nested worktree under `.claude/worktrees` goes with it.
        let mut topmost: Vec<PathBuf> = Vec::new();
        for path in found {
            if !topmost.iter().any(|kept| path.starts_with(kept)) {
                topmost.push(path);
            }
        }
        topmost
    }
}

/// The linked worktrees of the repository `root` belongs to that live inside `root` (but
/// are not `root` itself), relative to it. Git records each one's location in
/// `<common dir>/worktrees/<id>/gitdir` as the absolute path of its `.git` file.
fn nested_worktrees(root: &Path) -> Vec<PathBuf> {
    let Some(common) = common_dir(root) else {
        return Vec::new();
    };
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(common.join("worktrees")) else {
        return Vec::new();
    };
    let mut nested: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path().join("gitdir")).ok())
        .filter_map(|gitdir| {
            let dot_git = PathBuf::from(gitdir.trim());
            let worktree = dot_git.parent()?;
            let worktree = worktree
                .canonicalize()
                .unwrap_or_else(|_| worktree.to_path_buf());
            let relative = worktree.strip_prefix(&root).ok()?;
            (!relative.as_os_str().is_empty()).then(|| relative.to_path_buf())
        })
        .collect();
    nested.sort();
    nested.dedup();
    nested
}

/// The common Git directory of the repository whose working tree is `root`, if any.
fn common_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let meta = fs::symlink_metadata(&dot_git).ok()?;
    let git_dir = if meta.is_dir() {
        dot_git
    } else if meta.is_file() {
        let contents = fs::read_to_string(&dot_git).ok()?;
        let target = contents
            .lines()
            .find_map(|line| line.strip_prefix("gitdir:"))?;
        root.join(target.trim())
    } else {
        return None;
    };
    match fs::read_to_string(git_dir.join("commondir")) {
        Ok(relative) => Some(git_dir.join(relative.trim())),
        Err(_) => Some(git_dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_and_custom_patterns_apply_to_paths_and_their_children() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(FILE), "big/\n*.log\n").unwrap();
        let exclusions = Exclusions::of(root.path());
        assert!(exclusions.is_excluded(Path::new(".claude/worktrees"), true));
        assert!(exclusions.is_excluded(Path::new(".claude/worktrees/a/file.txt"), false));
        assert!(!exclusions.is_excluded(Path::new(".claude/settings.json"), false));
        assert!(exclusions.is_excluded(Path::new("big/x"), false));
        assert!(exclusions.is_excluded(Path::new("src/debug.log"), false));
        assert!(!exclusions.is_excluded(Path::new("src/main.rs"), false));
        assert!(!exclusions.is_excluded(Path::new(""), true));
    }

    #[test]
    fn a_custom_file_can_take_back_the_built_in_rule() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(FILE), "!.claude/worktrees/\n").unwrap();
        let exclusions = Exclusions::of(root.path());
        assert!(!exclusions.is_excluded(Path::new(".claude/worktrees/a"), true));
    }

    #[test]
    fn present_lists_only_topmost_existing_entries() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".claude/worktrees/a")).unwrap();
        let exclusions = Exclusions::of(root.path());
        assert_eq!(
            exclusions.present(root.path()),
            [PathBuf::from(".claude/worktrees")]
        );
        fs::write(root.path().join(FILE), "big/\n").unwrap();
        fs::create_dir_all(root.path().join("src/big/deep")).unwrap();
        let exclusions = Exclusions::of(root.path());
        assert_eq!(
            exclusions.present(root.path()),
            [PathBuf::from(".claude/worktrees"), PathBuf::from("src/big")]
        );
    }
}
