use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::diff::FileChange;

pub struct ChangeFilter {
    gitignore: Gitignore,
}

impl ChangeFilter {
    pub fn build(base: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(base);
        let _ = builder.add_line(None, "node_modules/");

        let gitignore_path = base.join(".gitignore");
        if gitignore_path.exists() {
            let _ = builder.add(&gitignore_path);
        }

        let forkspaceignore_path = base.join(".forkspaceignore");
        if forkspaceignore_path.exists() {
            let _ = builder.add(&forkspaceignore_path);
        }

        let gitignore = builder
            .build()
            .unwrap_or_else(|_| GitignoreBuilder::new(base).build().unwrap());

        Self { gitignore }
    }

    pub fn is_ignored(&self, rel_path: &Path, is_dir: bool) -> bool {
        self.gitignore
            .matched_path_or_any_parents(rel_path, is_dir)
            .is_ignore()
    }

    pub fn filter_changes(&self, changes: Vec<FileChange>) -> Vec<FileChange> {
        changes
            .into_iter()
            .filter(|c| !self.is_ignored(&c.path, false))
            .collect()
    }
}
