//! `forkspace undo`: put the project back as it was before the latest merge (of one
//! workspace, when named): its files, and the branch the merge kept commits in.

use crate::cli::UndoArgs;
use crate::error::Result;
use crate::state::State;
use crate::sync::{install, join_paths, undo};

pub fn exec(args: UndoArgs) -> Result {
    // The state lock keeps a merge from running into the project while it is restored.
    let report = State::with_state(|_| undo::undo(args.name.as_deref()))?;
    println!(
        "undid the merge of '{}' into {}: restored {} files, removed {} added files",
        report.workspace,
        report.project.display(),
        report.restored,
        report.removed
    );
    if let Some(branch) = &report.branch_reverted {
        println!("put branch '{branch}' back as it was before the merge");
    }
    if let Some(branch) = &report.branch_moved {
        eprintln!("warning: left branch '{branch}' alone because it moved after the merge");
    }
    if !report.changed_since.is_empty() {
        eprintln!(
            "warning: left alone because they changed after the merge (their pre-merge versions stay in the undo store): {}",
            join_paths(&report.changed_since)
        );
    }
    if !report.newer_merge.is_empty() {
        eprintln!(
            "warning: left alone because a newer merge also changed them; undo that merge first: {}",
            join_paths(&report.newer_merge)
        );
    }
    if install::touches_dependencies(&report.reverted) {
        if let Some(manager) = install::detect_in(&report.project) {
            install::run_install(&report.project, manager)?;
            println!("ran package install");
        }
    }
    Ok(())
}
