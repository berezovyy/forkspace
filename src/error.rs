use thiserror::Error;

#[derive(Debug, Error)]
pub enum ForkspaceError {
    #[error("workspace '{0}' not found (run 'forkspace ls' to see available workspaces)")]
    WorkspaceNotFound(String),

    #[error("workspace '{0}' already exists (use 'forkspace enter {0}' to enter it)")]
    WorkspaceExists(String),

    #[error("branch conflict: {0}")]
    BranchExists(String),

    #[error("workspace '{0}' has uncommitted tracked changes; commit them, or use `forkspace merge` for file-level transfer")]
    WorkspaceDirty(String),

    #[error("branch '{branch}' is checked out in the base; switch the base off it (`git switch -`) and re-run")]
    BranchCheckedOut { branch: String },

    #[error("workspace '{workspace}' has comparison base {base}, expected {expected}; cross-base columns would be misleading")]
    BaseMismatch {
        workspace: String,
        base: String,
        expected: String,
    },

    #[error("recorded {kind} path for workspace '{workspace}' is missing: {path}")]
    RecordedPathMissing {
        workspace: String,
        kind: String,
        path: String,
    },

    #[error("state operation failed: {0}")]
    StateFailed(String),

    #[error("state corrupted: {0}")]
    StateCorrupted(String),

    #[error("lock failed: {0}")]
    LockFailed(String),

    #[error("workspace '{workspace}' has children: {children:?} — use --force to remove all, or drop children first")]
    HasChildren {
        workspace: String,
        children: Vec<String>,
    },

    #[error("merge conflict: base changed since fork for {paths:?}; review the paths or rerun with --overwrite-base")]
    MergeConflict { paths: Vec<String> },

    #[error("pull conflict: base and workspace both changed {paths:?}; review the paths or rerun with --theirs")]
    PullConflict { paths: Vec<String> },

    #[error("workspace '{0}' has an interrupted pull; run `forkspace pull {0} --resume` before diff or merge")]
    StaleAncestor(String),

    #[error("mount failed: {0}")]
    MountFailed(String),

    #[error("unsupported capability: {diagnosis}; {remedy}")]
    CapabilityUnsupported { diagnosis: String, remedy: String },

    #[error("backend unavailable: {diagnosis}; {remedy}")]
    BackendUnavailable { diagnosis: String, remedy: String },

    #[error("unmount failed: {0}")]
    UnmountFailed(String),

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

    #[error("package manager failed: {0}")]
    PackageManagerFailed(String),

    #[error("git operation failed: {0}")]
    GitFailed(String),

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

impl ForkspaceError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::WorkspaceNotFound(_) => 3,
            Self::WorkspaceExists(_) => 4,
            Self::BranchExists(_) => 4,
            Self::HasChildren { .. }
            | Self::MergeConflict { .. }
            | Self::PullConflict { .. }
            | Self::WorkspaceDirty(_)
            | Self::BranchCheckedOut { .. }
            | Self::BaseMismatch { .. } => 5,
            Self::CapabilityUnsupported { .. } => 6,
            Self::BackendUnavailable { .. } | Self::MountFailed(_) => 7,
            Self::RecordedPathMissing { .. } => 3,
            _ => 1,
        }
    }
}

pub type Result<T = ()> = std::result::Result<T, ForkspaceError>;
