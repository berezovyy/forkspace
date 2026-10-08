//! Command-line grammar. Help text and flag names here are part of the CLI contract.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(
    version,
    about = "fork() for your project directory — disposable, isolated workspaces",
    long_about = "Forkspace creates disposable project workspaces using the fastest backend the host supports.\n\n\
        Linux overlayfs, APFS clones, Linux reflinks, and a portable copy floor share one \
        command and state contract. Backend selection is automatic and sticky per workspace."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new workspace from a directory or another workspace
    New(NewArgs),

    /// Remove a workspace and unmount its overlay
    Drop(DropArgs),

    /// Open a shell (or run a command) inside a workspace
    Enter(EnterArgs),

    /// Start a process in a workspace (dev server, Electron, etc.)
    Run(RunArgs),

    /// Switch the active workspace (hot-swap via proxy)
    Switch(SwitchArgs),

    /// List all active workspaces
    Ls(LsArgs),

    /// Show what changed in a workspace
    Diff(DiffArgs),

    /// Compare parallel workspace attempts side by side
    Compare(CompareArgs),

    /// Apply workspace changes back to the base directory
    Merge(MergeArgs),

    /// Bring base changes into a workspace
    Pull(PullArgs),

    /// Preserve a workspace's Git branch in the base repository
    Land(LandArgs),

    /// Generate shell completions
    #[command(hide = true)]
    Completions(CompletionsArgs),

    /// Internal: run the TCP proxy daemon
    #[command(name = "proxy-daemon", hide = true)]
    ProxyDaemon(ProxyDaemonArgs),
}

#[derive(Args)]
pub struct NewArgs {
    #[arg(help = "Name for the new workspace (defaults to the source directory's name)")]
    pub name: Option<String>,

    #[arg(
        long,
        value_name = "SOURCE",
        default_value = ".",
        help = "Directory or workspace to start from"
    )]
    pub from: PathBuf,

    #[arg(long, help = "Session identifier for grouping related workspaces")]
    pub session: Option<String>,

    #[arg(
        long,
        help = "Keep the workspace's changes in RAM (Linux overlay only; lost on reboot)"
    )]
    pub tmpfs: bool,

    #[arg(long, help = "Create and record a Git branch in the workspace")]
    pub branch: Option<String>,

    #[arg(long, help = "Consent to a full directory copy backend")]
    pub copy: bool,

    #[arg(
        long,
        conflicts_with = "copy",
        help = "Show later changes to the source in the workspace (Linux overlay only; skips the frozen copy)"
    )]
    pub live: bool,
}

#[derive(Args)]
pub struct DropArgs {
    #[arg(
        required_unless_present = "all",
        help = "Name of the workspace to remove"
    )]
    pub name: Option<String>,

    #[arg(long, help = "Force removal even if the workspace has children")]
    pub force: bool,

    #[arg(long, help = "Remove all workspaces")]
    pub all: bool,

    #[arg(
        long,
        help = "Treat name as a glob pattern to match multiple workspaces"
    )]
    pub glob: bool,
}

#[derive(Args)]
pub struct EnterArgs {
    #[arg(help = "Name of the workspace to enter (omit to auto-detect)")]
    pub name: Option<String>,

    #[arg(
        long,
        alias = "new",
        help = "Create the workspace if it does not exist"
    )]
    pub create: bool,

    #[arg(long, help = "Base directory when creating a new workspace on entry")]
    pub from: Option<String>,

    #[arg(long, help = "Destroy the workspace automatically on exit")]
    pub ephemeral: bool,

    #[arg(
        long,
        help = "Keep the workspace's changes in RAM (Linux overlay only; lost on reboot)"
    )]
    pub tmpfs: bool,

    #[arg(long, help = "Consent to a full directory copy backend when creating")]
    pub copy: bool,

    #[arg(
        long,
        conflicts_with = "copy",
        help = "When creating, show later changes to the base in the workspace (Linux overlay only)"
    )]
    pub live: bool,

    #[arg(
        long,
        help = "Merge changes back to the base directory when exiting the shell"
    )]
    pub merge_on_exit: bool,

    #[arg(long, help = "Session identifier to set in the spawned shell")]
    pub session: Option<String>,

    #[arg(
        last = true,
        help = "Command to run inside the workspace instead of a shell"
    )]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct RunArgs {
    #[arg(help = "Name of the workspace")]
    pub name: String,

    #[arg(
        long,
        default_value_t = 3000,
        help = "Port the app listens on inside the workspace"
    )]
    pub port: u16,

    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 10,
        help = "How long to wait for the app to listen before returning (0 to skip)"
    )]
    pub port_wait: u64,

    #[arg(long, help = "Stop the running process instead of starting one")]
    pub stop: bool,

    #[arg(last = true, help = "Command to run (e.g. npm run dev)")]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct SwitchArgs {
    #[arg(help = "Name of the workspace to switch to")]
    pub name: String,
}

#[derive(Args)]
pub struct LsArgs {
    #[arg(long, help = "Output only workspace names, one per line")]
    pub names: bool,

    #[arg(
        long,
        conflicts_with = "names",
        help = "Output workspace metadata as JSON"
    )]
    pub json: bool,

    #[arg(
        long,
        help = "Show all files including gitignored paths (build artifacts, node_modules, etc.)"
    )]
    pub no_gitignore: bool,
}

#[derive(Args)]
pub struct CompletionsArgs {
    #[arg(help = "Shell to generate completions for (bash, zsh, fish, elvish, powershell)")]
    pub shell: Shell,
}

#[derive(Args)]
pub struct DiffArgs {
    #[arg(help = "Name of the workspace to diff")]
    pub name: String,

    #[arg(long, conflicts_with_all = ["full", "files", "json"], help = "Show a compact stat summary (files changed, insertions, deletions)")]
    pub stat: bool,

    #[arg(long, conflicts_with_all = ["stat", "files", "json"], help = "Show the full unified diff output")]
    pub full: bool,

    #[arg(long, conflicts_with_all = ["stat", "full", "json"], help = "List only the changed file paths")]
    pub files: bool,

    #[arg(long, help = "Include changes from all ancestor layers")]
    pub cumulative: bool,

    #[arg(long, conflicts_with_all = ["stat", "full", "files"], help = "Output diff information as JSON")]
    pub json: bool,

    #[arg(
        long,
        help = "Show all files including gitignored paths (build artifacts, node_modules, etc.)"
    )]
    pub no_gitignore: bool,
}

#[derive(Args)]
pub struct CompareArgs {
    #[arg(num_args = 2.., help = "Two or more workspace names")]
    pub names: Vec<String>,

    #[arg(long, conflicts_with = "files", help = "Output stable JSON")]
    pub json: bool,

    #[arg(long, conflicts_with = "json", help = "Show the per-path matrix")]
    pub files: bool,
}

#[derive(Args)]
pub struct MergeArgs {
    #[arg(
        required_unless_present = "undo",
        help = "Name of the workspace to merge (with --undo: whose latest merge to undo)"
    )]
    pub name: Option<String>,

    #[arg(long, help = "Create a git commit with the merged changes")]
    pub commit: bool,

    #[arg(short, long, help = "Commit message (implies --commit)")]
    pub message: Option<String>,

    #[arg(long, help = "Generate a unified patch instead of merging")]
    pub patch: bool,

    #[arg(long, help = "Drop the workspace after a successful merge")]
    pub drop: bool,

    #[arg(long, help = "Skip automatic dependency installation after merge")]
    pub no_install: bool,

    #[arg(long, help = "Show the merge plan without applying it")]
    pub dry_run: bool,

    #[arg(long, help = "Resume an interrupted merge")]
    pub resume: bool,

    #[arg(
        long,
        conflicts_with = "drop",
        help = "Overwrite conflicting changes in the base"
    )]
    pub overwrite_base: bool,

    #[arg(long, hide = true, conflicts_with = "drop")]
    pub force: bool,

    #[arg(long, help = "Suppress the pre-apply plan summary")]
    pub quiet: bool,

    #[arg(
        long,
        conflicts_with_all = ["commit", "message", "patch", "drop", "dry_run", "resume", "overwrite_base"],
        help = "Put the base back as it was before the latest merge"
    )]
    pub undo: bool,

    #[arg(
        long,
        requires = "undo",
        help = "With --undo, list the merges that can be undone"
    )]
    pub list: bool,
}

#[derive(Args)]
pub struct PullArgs {
    #[arg(help = "Workspace name (defaults to $FORKSPACE_WORKSPACE)")]
    pub name: Option<String>,

    #[arg(long, help = "Show the pull plan without applying it")]
    pub dry_run: bool,

    #[arg(long, help = "Resolve conflicts by taking the base version")]
    pub theirs: bool,

    #[arg(long, help = "Resume an interrupted pull")]
    pub resume: bool,

    #[arg(long, help = "Skip automatic dependency installation")]
    pub no_install: bool,

    #[arg(long, help = "Output the pull plan as JSON")]
    pub json: bool,
}

#[derive(Args)]
pub struct LandArgs {
    #[arg(help = "Workspace whose commits should be landed")]
    pub name: String,

    #[arg(long, help = "Replace a divergent base branch")]
    pub force: bool,

    #[arg(long, help = "Drop the workspace after a successful land")]
    pub drop: bool,

    #[arg(
        long = "as",
        value_name = "BRANCH",
        help = "Land under a different branch name"
    )]
    pub as_branch: Option<String>,
}

#[derive(Args)]
pub struct ProxyDaemonArgs {
    #[arg(long, default_value_t = 4000, help = "Port to listen on")]
    pub port: u16,
}
