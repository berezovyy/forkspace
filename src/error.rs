//! The crate-wide error type and the process exit code each error maps to.
//!
//! Both the `Display` text and [`ForkspaceError::exit_code`] are part of the CLI contract.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ForkspaceError {
    // Workspace lookup and naming.
    #[error("workspace '{0}' not found (run 'forkspace ls' to see available workspaces)")]
    WorkspaceNotFound(String),

    #[error("workspace '{0}' already exists (use 'forkspace enter {0}' to enter it)")]
    WorkspaceExists(String),

    #[error("branch conflict: {0}")]
    BranchExists(String),

    #[error("recorded {kind} path for workspace '{workspace}' is missing: {path}")]
    RecordedPathMissing {
        workspace: String,
        kind: String,
        path: String,
    },

    // Refusals: the workspace or its base is not in a state where the command is safe.
    #[error("workspace '{workspace}' has children: {children:?} — use --force to remove all, or drop children first")]
    HasChildren {
        workspace: String,
        children: Vec<String>,
    },

    #[error("workspace '{0}' has uncommitted tracked changes; commit them, or use `forkspace merge` for file-level transfer")]
    WorkspaceDirty(String),

    #[error("workspace '{workspace}' is still used by: {processes}; {remedy}")]
    WorkspaceInUse {
        workspace: String,
        processes: String,
        remedy: String,
    },

    #[error("branch '{branch}' is checked out in the base; switch the base off it (`git switch -`) and re-run")]
    BranchCheckedOut { branch: String },

    #[error("workspace '{workspace}' has comparison base {base}, expected {expected}; cross-base columns would be misleading")]
    BaseMismatch {
        workspace: String,
        base: String,
        expected: String,
    },

    #[error("merge conflict: base changed since fork for {paths:?}; review the paths or rerun with --overwrite-base")]
    MergeConflict { paths: Vec<String> },

    #[error("pull conflict: base and workspace both changed {paths:?}; review the paths or rerun with --theirs")]
    PullConflict { paths: Vec<String> },

    #[error("merge conflict: the base and the workspace changed the same lines of {paths:?}; run `forkspace pull {workspace}`, resolve the conflict markers in the workspace, then merge again")]
    MergeOverlap {
        workspace: String,
        paths: Vec<String>,
    },

    #[error("workspace '{workspace}' has conflict markers to resolve in {paths:?}; edit those files in the workspace, then merge")]
    ConflictMarkers {
        workspace: String,
        paths: Vec<String>,
    },

    // Half-applied operations that must be resumed first.
    #[error("workspace '{0}' has an interrupted pull; run `forkspace pull {0} --resume` before diff or merge")]
    StaleAncestor(String),

    #[error("workspace '{workspace}' has an interrupted {operation}; {remedy}")]
    InterruptedOperation {
        workspace: String,
        operation: String,
        remedy: String,
    },

    // The state store.
    #[error("state operation failed: {0}")]
    StateFailed(String),

    #[error("state corrupted: {0}")]
    StateCorrupted(String),

    #[error("lock failed: {0}")]
    LockFailed(String),

    // Workspace backends.
    #[error("unsupported capability: {diagnosis}; {remedy}")]
    CapabilityUnsupported { diagnosis: String, remedy: String },

    #[error("backend unavailable: {diagnosis}; {remedy}")]
    BackendUnavailable { diagnosis: String, remedy: String },

    #[error("mount failed: {0}")]
    MountFailed(String),

    #[error("unmount failed: {0}")]
    UnmountFailed(String),

    // Dev-server processes and the proxy.
    #[error("process failed: {0}")]
    ProcessFailed(String),

    #[error("proxy failed: {0}")]
    ProxyFailed(String),

    #[error("no available ports in range 5501-5600")]
    PortRangeExhausted,

    #[error(
        "process already running in workspace '{0}' — stop it first with `forkspace run {0} --stop`"
    )]
    ProcessAlreadyRunning(String),

    #[error("no process running in workspace '{0}'")]
    NoProcessRunning(String),

    // External tools.
    #[error("package manager failed: {0}")]
    PackageManagerFailed(String),

    #[error("git operation failed: {0}")]
    GitFailed(String),

    // Generic.
    #[error("{0}")]
    InvalidArgument(String),

    #[error("serialization failed: {0}")]
    Serialization(String),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<serde_json::Error> for ForkspaceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error.to_string())
    }
}

impl ForkspaceError {
    /// The process exit status for this error.
    ///
    /// The match is exhaustive on purpose: a new variant must choose its code explicitly.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::WorkspaceNotFound(_) | Self::RecordedPathMissing { .. } => 3,
            Self::WorkspaceExists(_) | Self::BranchExists(_) => 4,
            Self::HasChildren { .. }
            | Self::WorkspaceDirty(_)
            | Self::WorkspaceInUse { .. }
            | Self::BranchCheckedOut { .. }
            | Self::BaseMismatch { .. }
            | Self::MergeConflict { .. }
            | Self::MergeOverlap { .. }
            | Self::ConflictMarkers { .. }
            | Self::PullConflict { .. } => 5,
            Self::CapabilityUnsupported { .. } => 6,
            Self::BackendUnavailable { .. } | Self::MountFailed(_) => 7,
            Self::StaleAncestor(_)
            | Self::InterruptedOperation { .. }
            | Self::StateFailed(_)
            | Self::StateCorrupted(_)
            | Self::LockFailed(_)
            | Self::UnmountFailed(_)
            | Self::ProcessFailed(_)
            | Self::ProxyFailed(_)
            | Self::PortRangeExhausted
            | Self::ProcessAlreadyRunning(_)
            | Self::NoProcessRunning(_)
            | Self::PackageManagerFailed(_)
            | Self::GitFailed(_)
            | Self::InvalidArgument(_)
            | Self::Serialization(_)
            | Self::Io { .. } => 1,
        }
    }
}

pub type Result<T = ()> = std::result::Result<T, ForkspaceError>;
