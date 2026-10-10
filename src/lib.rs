//! Forkspace: disposable, isolated project workspaces.
//!
//! Module map, from the outside in:
//!
//! - [`cli`] / [`commands`]: argument parsing and one thin handler per subcommand.
//! - [`workspace`]: the workspace record and its lifecycle (create, remove, guards).
//! - [`state`]: the durable `state.json` store, its lock, migrations, and crash recovery.
//! - [`backend`]: how a workspace view is materialized (overlay or directory snapshot).
//! - [`changes`]: what changed in a workspace, and how to render it.
//! - [`sync`]: moving changes between a workspace and its base (merge, pull).
//! - [`review`]: read-only facts about changes (signals, compare).
//! - [`process`]: process liveness and the dev-server proxy.
//! - [`disk`]: private disk use per workspace and store, and backup exclusion.
//! - [`git`], [`paths`], [`util`], [`error`]: shared infrastructure.

pub mod backend;
pub mod changes;
pub mod cli;
pub mod commands;
pub mod disk;
pub mod error;
pub mod git;
pub mod paths;
pub mod process;
pub mod review;
pub mod state;
pub mod sync;
pub mod util;
pub mod workspace;
