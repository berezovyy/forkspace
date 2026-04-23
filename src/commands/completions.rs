use std::io;

use clap::CommandFactory;
use clap_complete::{generate, Shell};

use crate::cli::Cli;
use crate::error::Result;

pub fn exec(shell: Shell) -> Result {
    match shell {
        Shell::Zsh => print!("{}", ZSH_COMPLETIONS),
        _ => {
            let mut cmd = Cli::command();
            generate(shell, &mut cmd, "forkspace", &mut io::stdout());
        }
    }
    Ok(())
}

const ZSH_COMPLETIONS: &str = r#"#compdef forkspace

_forkspace_workspaces() {
    local -a names
    names=(${(f)"$(forkspace ls --names 2>/dev/null)"})
    _describe 'workspace' names
}

_forkspace() {
    local -a commands
    commands=(
        'fork:Create a new workspace from a directory'
        'drop:Remove a workspace'
        'enter:Open a shell (or run a command) inside a workspace'
        'run:Start a process in a workspace'
        'switch:Switch the active workspace'
        'ls:List all active workspaces'
        'diff:Show what changed in a workspace'
        'compare:Compare parallel workspaces side by side'
        'merge:Apply workspace changes back to the base directory'
        'pull:Bring base changes into a workspace'
        'land:Preserve workspace commits in the base repository'
    )

    _arguments -C \
        '--help[Show help]' \
        '--version[Show version]' \
        '1:command:->cmd' \
        '*::arg:->args'

    case "$state" in
        cmd)
            _describe 'command' commands
            ;;
        args)
            case "$words[1]" in
                fork)
                    _arguments \
                        '1:base directory:_directories' \
                        '--name=[Workspace name]:name:' \
                        '--session=[Session identifier]:session:' \
                        '--tmpfs[Back with tmpfs]' \
                        '--size=[tmpfs size limit]:size:' \
                        '--branch=[Create a Git branch]:branch:' \
                        '--copy[Consent to a full directory copy]' \
                        '--help[Show help]'
                    ;;
                drop)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--force[Force removal]' \
                        '--all[Remove all workspaces]' \
                        '--glob[Treat name as glob pattern]' \
                        '--help[Show help]'
                    ;;
                enter)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--create[Create if not exists]' \
                        '--from=[Base directory]:directory:_directories' \
                        '--ephemeral[Destroy on exit]' \
                        '--tmpfs[Back with tmpfs]' \
                        '--copy[Consent to a full directory copy]' \
                        '--merge-on-exit[Merge changes on exit]' \
                        '--session=[Session identifier]:session:' \
                        '--help[Show help]'
                    ;;
                run)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--port=[App port]:port:' \
                        '--stop[Stop running process]' \
                        '--help[Show help]'
                    ;;
                switch)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--help[Show help]'
                    ;;
                ls)
                    _arguments \
                        '--names[Output only workspace names]' \
                        '--json[Output workspace metadata as JSON]' \
                        '--no-gitignore[Include ignored files]' \
                        '--help[Show help]'
                    ;;
                diff)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--stat[Show compact stat summary]' \
                        '--full[Show full unified diff]' \
                        '--files[List changed file paths]' \
                        '--cumulative[Include ancestor layers]' \
                        '--json[Output as JSON]' \
                        '--no-gitignore[Include ignored files]' \
                        '--help[Show help]'
                    ;;
                compare)
                    _arguments \
                        '*:workspace:_forkspace_workspaces' \
                        '--json[Output stable JSON]' \
                        '--files[Show the per-path matrix]' \
                        '--help[Show help]'
                    ;;
                merge)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--commit[Create a git commit]' \
                        '-m=[Commit message]:message:' \
                        '--message=[Commit message]:message:' \
                        '--patch[Generate unified patch]' \
                        '--drop[Drop workspace after merge]' \
                        '--no-install[Skip dependency installation]' \
                        '--dry-run[Show the merge plan without applying]' \
                        '--resume[Resume an interrupted merge]' \
                        '--overwrite-base[Overwrite conflicting base changes]' \
                        '--quiet[Suppress the pre-apply plan summary]' \
                        '--help[Show help]'
                    ;;
                pull)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--dry-run[Show the pull plan without applying]' \
                        '--theirs[Resolve conflicts with the base version]' \
                        '--resume[Resume an interrupted pull]' \
                        '--no-install[Skip dependency installation]' \
                        '--json[Output the pull plan as JSON]' \
                        '--help[Show help]'
                    ;;
                land)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--force[Replace a divergent branch]' \
                        '--drop[Drop after a successful land]' \
                        '--as=[Land under another branch]:branch:' \
                        '--help[Show help]'
                    ;;
            esac
            ;;
    esac
}

_forkspace "$@"
"#;
