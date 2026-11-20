//! The crate-wide error type and the process exit code each error maps to.
//!
//! Both the `Display` text and [`ForkspaceError::exit_code`] are part of the CLI contract.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ForkspaceError {
    /// The command `enter` ran failed; forkspace exits with the same code. Its message was
    /// already printed when the command ended.
    #[error("the command exited with code {0}")]
    CommandExited(i32),

    // Workspace lookup and naming.
    #[error("there is no workspace '{0}' (`forkspace ls` lists them)")]
    WorkspaceNotFound(String),

    #[error("a workspace named '{0}' already exists (`forkspace enter {0}` opens it)")]
    WorkspaceExists(String),

    #[error("branch conflict: {0}")]
    BranchExists(String),

    #[error("the {kind} of workspace '{workspace}' is missing: {path}")]
    RecordedPathMissing {
        workspace: String,
        kind: String,
        path: String,
    },

    // Refusals: the workspace or its base is not in a state where the command is safe.
    #[error("workspaces were made from '{workspace}': {}; drop them too, in the same command or first", children.join(", "))]
    HasChildren {
        workspace: String,
        children: Vec<String>,
    },

    #[error("workspace '{workspace}' is still used by: {processes}; {remedy}")]
    WorkspaceInUse {
        workspace: String,
        processes: String,
        remedy: String,
    },

    #[error("'{workspace}' was made from {base}, the others from {expected}; compare workspaces of the same project")]
    BaseMismatch {
        workspace: String,
        base: String,
        expected: String,
    },

    #[error("can't merge: your project also changed {} since the workspace was made; run `forkspace pull` to bring those changes into the workspace, settle them there, then merge again", paths.join(", "))]
    MergeConflict { paths: Vec<String> },

    #[error("cannot export a complete patch of '{workspace}', so none was printed: {}; apply these changes with `forkspace merge {workspace}` instead", omitted.join("; "))]
    PatchIncomplete {
        workspace: String,
        omitted: Vec<String>,
    },

    #[error("can't pull: your project and the workspace both changed {}; settle them in the workspace by hand, then pull again", paths.join(", "))]
    PullConflict { paths: Vec<String> },

    #[error("can't merge: your project and the workspace changed the same lines of {}; run `forkspace pull {workspace}`, fix the conflict markers in the workspace, then merge again", paths.join(", "))]
    MergeOverlap {
        workspace: String,
        paths: Vec<String>,
    },

    #[error("'{workspace}' still has conflict markers in {}; fix those files in the workspace, then merge", paths.join(", "))]
    ConflictMarkers {
        workspace: String,
        paths: Vec<String>,
    },

    /// A merge or pull finished, but left these paths as they were because they changed while
    /// it ran.
    #[error("{operation} left {} as they are: {remedy}", paths.join(", "))]
    ChangedDuring {
        operation: String,
        paths: Vec<String>,
        remedy: String,
    },

    // Half-applied operations that must be resumed first.
    #[error("workspace '{0}' has an unfinished pull; run `forkspace pull {0}` to finish it")]
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
    #[error("can't do that here: {diagnosis}; {remedy}")]
    CapabilityUnsupported { diagnosis: String, remedy: String },

    /// A project path fuse-overlayfs cannot hold; `new` copies such a project instead.
    #[error("{0} starts with .wh., a name forkspace cannot keep in a workspace; rename it")]
    ReservedOverlayName(String),

    #[error("can't make workspaces here: {diagnosis}; {remedy}")]
    BackendUnavailable { diagnosis: String, remedy: String },

    #[error("could not open the workspace folder: {0}")]
    MountFailed(String),

    #[error("could not close the workspace folder: {0}")]
    UnmountFailed(String),

    // Dev-server processes and the proxy.
    #[error("process failed: {0}")]
    ProcessFailed(String),

    #[error("proxy failed: {0}")]
    ProxyFailed(String),

    #[error("no free port left between 5501 and 5600; stop a dev server with `forkspace run <name> --stop`")]
    PortRangeExhausted,

    #[error("a dev server already runs in '{0}'; stop it first with `forkspace run {0} --stop`")]
    ProcessAlreadyRunning(String),

    #[error("no dev server runs in '{0}'")]
    NoProcessRunning(String),

    // External tools.
    #[error("installing dependencies failed: {0}")]
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
            Self::CommandExited(code) => *code,
            Self::WorkspaceNotFound(_) | Self::RecordedPathMissing { .. } => 3,
            Self::WorkspaceExists(_) | Self::BranchExists(_) => 4,
            Self::HasChildren { .. }
            | Self::WorkspaceInUse { .. }
            | Self::BaseMismatch { .. }
            | Self::MergeConflict { .. }
            | Self::MergeOverlap { .. }
            | Self::PatchIncomplete { .. }
            | Self::ConflictMarkers { .. }
            | Self::ChangedDuring { .. }
            | Self::PullConflict { .. } => 5,
            Self::CapabilityUnsupported { .. } | Self::ReservedOverlayName(_) => 6,
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
