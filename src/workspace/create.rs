//! Creating a workspace: resolve the origin, pick a backend, and materialize the view.
//!
//! Creation is recorded as a pending intent in state before any files are written, so a
//! crash mid-materialization leaves a record that recovery can clean up.

use std::path::PathBuf;

use crate::backend::{
    choose_backend_for_create, pending_create_path, select_backend, select_backend_for_create,
    CreateReq, DestroyMode, OverlayState, WorkspaceBackend,
};
use crate::changes::manifest::{Builder, ManifestStatus};
use crate::error::{ForkspaceError, Result};
use crate::process::watch;
use crate::state::{PendingCreate, State};
use crate::util::{trace, IoContext};
use crate::workspace::{guard, Workspace};

/// Stacks deeper than this are refused; merge or fork from an earlier ancestor instead.
const MAX_FORK_DEPTH: usize = 32;

#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub session: Option<String>,
    /// Keep the writable layers in RAM; only the overlay backend supports it.
    pub tmpfs: bool,
    /// Skip capability probing and use the portable full-copy backend.
    pub force_copy: bool,
    /// Overlay only: mount the base itself rather than a frozen copy of it.
    pub live: bool,
    /// Create this Git branch in the view and record it; the fork fails without it.
    pub branch: Option<String>,
    /// Overlay only: do not flush the workspace's writes on `fsync`.
    pub no_fsync: bool,
}

/// Where a new workspace's contents come from, and the backend that will copy them.
struct Origin {
    path: PathBuf,
    parent: Option<String>,
    /// The parent's overlay layers, when stacking on an overlay workspace.
    parent_layers: Option<OverlayState>,
    backend: Box<dyn WorkspaceBackend>,
    /// Why faster backends were passed over, when they were.
    skipped: Option<String>,
}

/// Fork `base` into a new workspace called `name`.
///
/// If `base` is the name of an existing workspace, the new workspace is stacked on top of it
/// and inherits its backend (unless `force_copy` is set).
pub fn create(base: PathBuf, name: &str, options: CreateOptions) -> Result<Workspace> {
    validate_name(name)?;
    let preflight = trace::span("preflight");
    let Origin {
        path: origin,
        parent,
        parent_layers,
        backend,
        skipped,
    } = resolve_origin(base, name, options.force_copy)?;

    backend.preflight(&origin)?;
    let origin = origin
        .canonicalize()
        .io_context(|| format!("failed to canonicalize base path: {}", origin.display()))?;
    drop(preflight);

    record_intent(name, backend.as_ref())?;
    let request = CreateReq {
        name: name.to_string(),
        base: origin.clone(),
        tmpfs: options.tmpfs,
        live: options.live,
        no_fsync: options.no_fsync,
        parent_layers,
    };
    let view = match backend.create(&request) {
        Ok(view) => view,
        Err(error) => {
            State::with_state(|state| {
                clear_intent(state, name);
                Ok(())
            })?;
            return Err(explain_long_path(error));
        }
    };

    let mut workspace = Workspace::from_view(
        name.to_string(),
        origin,
        parent,
        options.session,
        backend.kind(),
        view,
    );
    if let Some(skipped) = skipped {
        workspace.probe_reason = Some(match workspace.probe_reason.take() {
            Some(reason) => format!("{reason} (passed over {skipped})"),
            None => format!("passed over {skipped}"),
        });
    }
    record_fork_time_files(&workspace);
    let prepared = give_private_git(&mut workspace)
        .and_then(|()| create_branch(&mut workspace, options.branch.as_deref()));
    if let Err(error) = prepared {
        let _ = backend.destroy(&workspace, DestroyMode::Normal);
        let _ = crate::changes::manifest::remove(name);
        State::with_state(|state| {
            clear_intent(state, name);
            Ok(())
        })?;
        return Err(error);
    }
    // The manifest builder starts only once the workspace is recorded, under the same lock, so
    // it never waits for the record (STATE-02); its process is recorded so a builder that dies
    // is restarted. Without a builder the manifest is built here instead.
    let in_background = workspace.manifest_status == ManifestStatus::Building;
    let build_here = trace::time("state", || {
        State::with_state(|state| {
            clear_intent(state, name);
            state.add_workspace(workspace.clone())?;
            if in_background && crate::changes::manifest::start_background(state, name).is_err() {
                state.require_workspace_mut(name)?.manifest_builder =
                    Some(Builder::of(std::process::id()));
                return Ok(true);
            }
            Ok(false)
        })
    })?;
    if build_here {
        if let Err(error) = crate::changes::manifest::build_here(name) {
            eprintln!("warning: could not record the fork manifest of '{name}' ({error}); merge and pull will try again");
        }
    }
    super::git_index::start(&workspace);
    Ok(workspace)
}

/// A project path that fits the system's path limit where the project is can exceed it once
/// placed under `FORKSPACE_HOME`, which may be the longer of the two. Say so, rather than
/// print the whole path with "File name too long".
fn explain_long_path(error: ForkspaceError) -> ForkspaceError {
    match error {
        ForkspaceError::Io { context, source }
            if source.raw_os_error() == Some(libc::ENAMETOOLONG) =>
        {
            let home = crate::paths::forkspace_home();
            let shown: String = context.chars().take(160).collect();
            ForkspaceError::InvalidArgument(format!(
                "a path in the project is too long to copy under FORKSPACE_HOME ({}, {} bytes): {shown}…; set FORKSPACE_HOME to a shorter directory, or exclude that path in .forkspaceexclude",
                home.display(),
                home.as_os_str().len()
            ))
        }
        error => error,
    }
}

/// Check out a new `branch` in the view and record it, before the workspace is saved, so
/// `--branch` is all-or-nothing and costs no separate state write.
fn create_branch(workspace: &mut Workspace, branch: Option<&str>) -> Result {
    let Some(branch) = branch else {
        return Ok(());
    };
    if crate::git::switch_new_branch(&workspace.view_path, branch).is_err() {
        return Err(ForkspaceError::BranchExists(format!(
            "'{branch}' could not be created; no workspace was kept"
        )));
    }
    workspace.branch = Some(branch.to_string());
    Ok(())
}

/// Keep the fork-time version of a snapshot workspace's source files for line-level merges.
/// Without it, files both sides change are whole-file conflicts, so a failure only warns.
fn record_fork_time_files(workspace: &Workspace) {
    if !workspace.backend_kind.stacking_is_snapshot() {
        return;
    }
    let recorded = trace::time("ancestor", || {
        crate::sync::ancestor::record(&workspace.name, &workspace.view_path)
    });
    if let Err(error) = recorded {
        eprintln!(
            "warning: could not keep fork-time copies for line-level merges ({error}); files both sides change will be whole-file conflicts"
        );
    }
}

/// When the base's `.git` points outside it (a linked worktree or submodule), give the view a
/// repository of its own, so commits in the workspace cannot move the project's branches.
///
/// Also records the `HEAD` commit at fork time, so `merge` can tell cheaply whether the
/// workspace made commits that a file-level merge would flatten.
fn give_private_git(workspace: &mut Workspace) -> Result {
    let private = super::private_git::privatize(&workspace.base, &workspace.view_path)?;
    if private || workspace.view_path.join(".git").exists() {
        workspace.git_head_at_fork = crate::git::head_commit(&workspace.view_path).ok();
    }
    Ok(())
}

/// Resolve `base` to a directory, stacking on the workspace of that name if there is one.
fn resolve_origin(base: PathBuf, name: &str, force_copy: bool) -> Result<Origin> {
    let state = State::load()?;
    ensure_name_free(&state, name)?;

    let Some(parent) = state.workspaces.get(base.to_str().unwrap_or("")) else {
        let (backend, skipped) = choose_backend_for_create(&base, force_copy)?;
        return Ok(Origin {
            path: base,
            parent: None,
            parent_layers: None,
            backend,
            skipped,
        });
    };

    guard::ensure_idle(&parent.name)?;
    if state.depth_of(&parent.name) + 1 >= MAX_FORK_DEPTH {
        return Err(ForkspaceError::InvalidArgument(format!(
            "workspace stack depth is limited to {MAX_FORK_DEPTH}; merge or fork from an earlier ancestor"
        )));
    }
    guard::require_live_view(&state, parent)?;
    let path = select_backend(Some(parent.backend_kind))?.view_path(parent);
    let backend = if force_copy {
        select_backend_for_create(&path, true)?
    } else {
        select_backend(Some(parent.backend_kind))?
    };
    Ok(Origin {
        path,
        parent: Some(parent.name.clone()),
        parent_layers: parent.linux_state().ok().cloned(),
        backend,
        skipped: None,
    })
}

/// Persist the intent to create `name`, re-checking the name under the lock.
fn record_intent(name: &str, backend: &dyn WorkspaceBackend) -> Result {
    let intent = PendingCreate {
        name: name.to_string(),
        path: pending_create_path(backend.kind(), name),
        backend_kind: backend.kind(),
        pid: std::process::id(),
        identity: watch::identity(std::process::id()),
        unknown_fields: serde_json::Map::new(),
    };
    State::with_state(|state| {
        ensure_name_free(state, name)?;
        state.pending_creates.push(intent);
        Ok(())
    })
}

fn clear_intent(state: &mut State, name: &str) {
    state.pending_creates.retain(|pending| pending.name != name);
}

fn ensure_name_free(state: &State, name: &str) -> Result {
    if state.name_in_use(name) {
        return Err(ForkspaceError::WorkspaceExists(name.to_string()));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result {
    let invalid = name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.bytes().any(|byte| byte == 0 || byte == b'/');
    if invalid {
        return Err(ForkspaceError::InvalidArgument(
            "invalid workspace name; use 1-255 bytes without '/', NUL, '.' or '..'".to_string(),
        ));
    }
    // An overlay workspace lives at `FORKSPACE_HOME/<name>`, beside the stores (SEC-02).
    if crate::paths::is_reserved_name(name) {
        return Err(ForkspaceError::InvalidArgument(format!(
            "workspace name '{name}' is reserved: forkspace uses it (or any name starting with '.') for its own files"
        )));
    }
    Ok(())
}
