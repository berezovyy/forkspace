//! Preconditions checked before a command touches a workspace.

use crate::error::{ForkspaceError, Result};
use crate::sync::journal::{self, OperationKind};
use crate::workspace::Workspace;

/// Refuse to proceed while a merge or pull for `name` is half-applied.
pub fn ensure_idle(name: &str) -> Result {
    ensure_no_interrupted_merge(name)?;
    ensure_no_interrupted_pull(name)
}

/// Refuse while a merge of `name` into its base is half-applied.
pub fn ensure_no_interrupted_merge(name: &str) -> Result {
    if journal::find_open(OperationKind::Merge, name)?.is_some() {
        return Err(ForkspaceError::InterruptedOperation {
            workspace: name.to_string(),
            operation: "merge".to_string(),
            remedy: format!("run `forkspace merge {name} --resume`"),
        });
    }
    Ok(())
}

/// Refuse while a pull into `name` is half-applied: its view mixes two base versions.
pub fn ensure_no_interrupted_pull(name: &str) -> Result {
    if journal::find_open(OperationKind::Pull, name)?.is_some() {
        return Err(ForkspaceError::StaleAncestor(name.to_string()));
    }
    Ok(())
}

/// The recorded view directory must still exist.
pub fn require_view(workspace: &Workspace) -> Result {
    if workspace.view_path.is_dir() {
        return Ok(());
    }
    Err(ForkspaceError::RecordedPathMissing {
        workspace: workspace.name.clone(),
        kind: "view".to_string(),
        path: workspace.view_path.display().to_string(),
    })
}
