//! Command-line grammar. Help text and flag names here are part of the CLI contract.

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
    /// Copy the project you are in into a new workspace
    New(NewArgs),

    /// Remove a workspace and unmount its overlay
    Drop(DropArgs),

    /// Open a shell (or run a command) inside a workspace
    Enter(EnterArgs),

    /// Start a process in a workspace (dev server, Electron, etc.)
    Run(RunArgs),

    /// Switch the active workspace (hot-swap via proxy)
    Switch(SwitchArgs),

    /// List your workspaces: what changed, how old, what runs
    Ls(LsArgs),

    /// Show what changed in a workspace, or compare several
    Diff(DiffArgs),

    /// Put a workspace's changes into your project (and keep its commits as a branch)
    Merge(MergeArgs),

    /// Put your project back as it was before the latest merge
    Undo(UndoArgs),

    /// Bring base changes into a workspace
    Pull(PullArgs),

    /// Save the current state of a workspace
    Checkpoint(CheckpointArgs),

    /// List a workspace's checkpoints
    Checkpoints(CheckpointsArgs),

    /// Return a workspace to one of its checkpoints
    Restore(RestoreArgs),

    /// Show or change a workspace's memory and CPU limits
    Limit(LimitArgs),

    /// Show what disk forkspace could give back, and reclaim it
    Gc(GcArgs),

    /// Upgrade forkspace to the latest release (or to a given version)
    #[command(visible_alias = "update")]
    Upgrade(UpgradeArgs),

    /// Print instructions for coding agents that use forkspace
    Skill,

    /// Run as a Claude Code hook, so its worktrees are forkspace workspaces
    Hook(HookArgs),

    /// Generate shell completions
    #[command(hide = true)]
    Completions(CompletionsArgs),

    /// Internal: run the TCP proxy daemon
    #[command(name = "proxy-daemon", hide = true)]
    ProxyDaemon(ProxyDaemonArgs),
}

#[derive(Args)]
pub struct NewArgs {
    #[arg(help = "Name for the new workspace (defaults to <project>-1, <project>-2, ...)")]
    pub name: Option<String>,

    #[arg(
        long,
        value_name = "SIZE",
        help = "Throttle the workspace's processes above this much memory, e.g. 4G (Linux)"
    )]
    pub memory: Option<String>,

    #[arg(
        long,
        value_name = "PERCENT",
        help = "Cap the workspace's CPU time, as a percentage of one CPU, e.g. 200% (Linux)"
    )]
    pub cpu: Option<String>,
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

    #[arg(long, help = "Stop processes still running in the workspace")]
    pub kill: bool,

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
    #[arg(
        help = "Workspace (defaults to the one you are in; made from your project if it does not exist)"
    )]
    pub name: Option<String>,

    #[arg(
        last = true,
        help = "Command to run inside the workspace instead of a shell"
    )]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct RunArgs {
    #[arg(help = "Workspace (defaults to the one you are in)")]
    pub name: Option<String>,

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

    #[arg(long, help = "Let the process write outside its workspace")]
    pub no_sandbox: bool,

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
    #[arg(long, help = "Print the workspaces as JSON, for tools")]
    pub json: bool,
}

#[derive(Args)]
pub struct CompletionsArgs {
    #[arg(help = "Shell to generate completions for (bash, zsh, fish, elvish, powershell)")]
    pub shell: Shell,
}

#[derive(Args)]
pub struct DiffArgs {
    #[arg(help = "Workspace (defaults to the one you are in); name two or more to compare them")]
    pub names: Vec<String>,

    #[arg(long, help = "Print the changes as JSON, for tools")]
    pub json: bool,
}

#[derive(Args)]
pub struct MergeArgs {
    #[arg(help = "Workspace (defaults to the one you are in)")]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct UndoArgs {
    #[arg(help = "Undo the latest merge of this workspace (default: the latest merge of any)")]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct GcArgs {
    #[arg(
        long,
        value_name = "AGE",
        default_value = "7d",
        help = "Only remove unchanged workspaces older than this (s, m, h, d, w)"
    )]
    pub older_than: String,

    #[arg(long, help = "Remove without asking")]
    pub yes: bool,
}

#[derive(Args)]
pub struct LimitArgs {
    #[arg(help = "Name of the workspace")]
    pub name: String,

    #[arg(
        long,
        value_name = "SIZE",
        help = "Memory above which it is throttled, e.g. 4G; `none` removes it"
    )]
    pub memory: Option<String>,

    #[arg(
        long,
        value_name = "PERCENT",
        help = "CPU time cap as a percentage of one CPU; `none` removes it"
    )]
    pub cpu: Option<String>,
}

#[derive(Args)]
pub struct CheckpointArgs {
    #[arg(help = "Workspace (defaults to the one you are in)")]
    pub name: Option<String>,

    #[arg(short, long, help = "A label to recognize the checkpoint by")]
    pub message: Option<String>,

    #[arg(long, help = "Also save ignored paths such as node_modules")]
    pub all: bool,
}

#[derive(Args)]
pub struct CheckpointsArgs {
    #[arg(help = "Workspace (defaults to the one you are in)")]
    pub name: Option<String>,

    #[arg(long, help = "Output as JSON")]
    pub json: bool,
}

#[derive(Args)]
pub struct RestoreArgs {
    #[arg(help = "Name of the workspace to restore")]
    pub name: String,

    #[arg(
        required_unless_present = "resume",
        help = "Checkpoint id (see `forkspace checkpoints`)"
    )]
    pub id: Option<u64>,

    #[arg(long, conflicts_with = "id", help = "Finish an interrupted restore")]
    pub resume: bool,

    #[arg(
        long,
        help = "Skip dependency installation when the restore changes package.json, a lockfile or package-manager settings"
    )]
    pub no_install: bool,
}

#[derive(Args)]
pub struct PullArgs {
    #[arg(help = "Workspace (defaults to the one you are in)")]
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
pub struct HookArgs {
    #[command(subcommand)]
    pub event: HookEvent,
}

#[derive(Subcommand)]
pub enum HookEvent {
    /// WorktreeCreate: fork the project and print the workspace's path
    WorktreeCreate,
    /// WorktreeRemove: drop the workspace, keeping one with unmerged work
    WorktreeRemove {
        #[arg(
            long,
            help = "Drop the workspace even when it has unmerged changes or commits"
        )]
        discard: bool,
    },
    /// Print the settings.json hooks that enable the two hooks above
    ClaudeSettings,
}

#[derive(Args)]
pub struct UpgradeArgs {
    #[arg(help = "Version to install, e.g. 0.2.0 (defaults to the latest release)")]
    pub version: Option<String>,

    #[arg(
        long,
        conflicts_with = "version",
        help = "Only report whether a newer release exists"
    )]
    pub check: bool,
}

#[derive(Args)]
pub struct ProxyDaemonArgs {
    #[arg(long, default_value_t = 4000, help = "Port to listen on")]
    pub port: u16,
}
