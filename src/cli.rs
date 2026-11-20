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

    /// Throw workspaces away (and stop what runs in them)
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

    /// Bring your project's newer changes into a workspace
    Pull(PullArgs),

    /// Save a snapshot of a workspace to go back to
    Save(SaveArgs),

    /// Go back to a snapshot (without one, list them)
    Restore(RestoreArgs),

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
}

#[derive(Args)]
pub struct DropArgs {
    #[arg(required = true, help = "Workspaces to throw away")]
    pub names: Vec<String>,
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
pub struct SaveArgs {
    #[arg(
        value_name = "NAME",
        help = "Workspace (defaults to the one you are in)"
    )]
    pub first: Option<String>,

    #[arg(value_name = "NOTE", help = "A note to recognize the snapshot by")]
    pub second: Option<String>,
}

#[derive(Args)]
pub struct RestoreArgs {
    #[arg(
        value_name = "NAME",
        help = "Workspace (defaults to the one you are in)"
    )]
    pub first: Option<String>,

    #[arg(
        value_name = "ID",
        help = "The snapshot to go back to (without one, list them)"
    )]
    pub second: Option<String>,
}

#[derive(Args)]
pub struct PullArgs {
    #[arg(help = "Workspace (defaults to the one you are in)")]
    pub name: Option<String>,

    #[arg(long, help = "Print the pull plan as JSON, for tools")]
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
