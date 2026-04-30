//! Cumulative changes of a stacked workspace relative to the root of its parent chain.
//!
//! Every path touched by any layer is re-classified by comparing the final view against the
//! root base, so a change made in one layer and reverted in another drops out.

use std::collections::BTreeSet;
use std::fs;

use super::content::classify_file;
use super::FileChange;
use crate::backend::select_backend;
use crate::error::Result;
use crate::state::State;
use crate::workspace::Workspace;

pub fn collect_cumulative_changes(workspace: &Workspace, state: &State) -> Result<Vec<FileChange>> {
    collect(workspace, state, true)
}

/// Like [`collect_cumulative_changes`], without line counts or binary detection.
pub fn collect_cumulative_changes_fast(
    workspace: &Workspace,
    state: &State,
) -> Result<Vec<FileChange>> {
    collect(workspace, state, false)
}

fn collect(
    workspace: &Workspace,
    state: &State,
    need_line_counts: bool,
) -> Result<Vec<FileChange>> {
    let chain = state.parent_chain(&workspace.name);
    let Some(root) = chain.first() else {
        return Ok(Vec::new());
    };

    let mut touched = BTreeSet::new();
    for layer in &chain {
        for change in select_backend(Some(layer.backend_kind))?.changes(layer, true, false)? {
            touched.insert(change.path);
        }
    }

    let final_view = select_backend(Some(workspace.backend_kind))?.view_path(workspace);
    let mut changes = Vec::new();
    for path in touched {
        let base_path = root.base.join(&path);
        let final_path = final_view.join(&path);
        match (
            fs::symlink_metadata(&base_path),
            fs::symlink_metadata(&final_path),
        ) {
            (Ok(_), Err(_)) => changes.push(FileChange::deletion(path)),
            (Err(_), Err(_)) => {}
            (_, Ok(metadata)) if metadata.is_dir() => {}
            (_, Ok(_)) => changes.extend(classify_file(
                path,
                &final_path,
                &base_path,
                need_line_counts,
            )),
        }
    }

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}
