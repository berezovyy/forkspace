//! Cumulative changes of a stacked workspace relative to the root of its parent chain.

use std::collections::BTreeSet;
use std::fs;

use super::content::classify_file;
use super::FileChange;
use crate::backend::select_backend;
use crate::error::Result;
use crate::state::State;
use crate::workspace::Workspace;

/// Replays layer stacking across the parent chain to compute cumulative changes
/// relative to the root base directory.
pub fn collect_cumulative_changes_fast(
    workspace: &Workspace,
    state: &State,
) -> Result<Vec<FileChange>> {
    collect_cumulative_changes_inner(workspace, state, false)
}

pub fn collect_cumulative_changes(workspace: &Workspace, state: &State) -> Result<Vec<FileChange>> {
    collect_cumulative_changes_inner(workspace, state, true)
}

fn collect_cumulative_changes_inner(
    workspace: &Workspace,
    state: &State,
    need_line_counts: bool,
) -> Result<Vec<FileChange>> {
    let chain = state.parent_chain(&workspace.name);
    if chain.is_empty() {
        return Ok(Vec::new());
    }

    let original_base = &chain[0].base;
    let mut touched = BTreeSet::new();
    for layer in &chain {
        for change in select_backend(Some(layer.backend_kind))?.changes(layer, true, false)? {
            touched.insert(change.path);
        }
    }
    let final_view = select_backend(Some(workspace.backend_kind))?.view_path(workspace);
    let mut changes = Vec::new();
    for path in touched {
        let base_path = original_base.join(&path);
        let final_path = final_view.join(&path);
        match (
            fs::symlink_metadata(&base_path),
            fs::symlink_metadata(&final_path),
        ) {
            (Ok(_), Err(_)) => changes.push(FileChange::deletion(path)),
            (Err(_), Err(_)) => {}
            (_, Ok(metadata)) if metadata.is_dir() => {}
            (_, Ok(_)) => {
                if let Some(change) = classify_file(path, &final_path, &base_path, need_line_counts)
                {
                    changes.push(change);
                }
            }
        }
    }

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}
