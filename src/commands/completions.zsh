#compdef forkspace

_forkspace_workspaces() {
    local -a names
    names=(${(f)"$(forkspace ls --json 2>/dev/null | sed -n 's/^    "name": "\(.*\)",$/\1/p')"})
    _describe 'workspace' names
}

_forkspace() {
    local -a commands
    commands=(
        'new:Create a new workspace from a directory or another workspace'
        'drop:Remove a workspace'
        'enter:Open a shell (or run a command) inside a workspace'
        'run:Start a process in a workspace'
        'switch:Switch the active workspace'
        'ls:List all active workspaces'
        'diff:Show what changed in a workspace'
        'merge:Apply workspace changes back to the base directory'
        'pull:Bring base changes into a workspace'
        'checkpoint:Save the current state of a workspace'
        'checkpoints:List a workspace'"'"'s checkpoints'
        'restore:Return a workspace to one of its checkpoints'
        'limit:Show or change a workspace'"'"'s memory and CPU limits'
        'gc:Show what disk forkspace could give back, and reclaim it'
        'upgrade:Upgrade forkspace to the latest release'
        'update:Upgrade forkspace to the latest release'
        'skill:Print instructions for coding agents that use forkspace'
        'hook:Run as a Claude Code hook'
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
                new)
                    _arguments \
                        '1:workspace name:' \
                        '--memory=[Memory above which the workspace is throttled]:size:' \
                        '--cpu=[CPU time cap, percent of one CPU]:percent:' \
                        '--help[Show help]'
                    ;;
                drop)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--force[Force removal]' \
                        '--kill[Stop processes still running in the workspace]' \
                        '--all[Remove all workspaces]' \
                        '--glob[Treat name as glob pattern]' \
                        '--help[Show help]'
                    ;;
                enter)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--help[Show help]'
                    ;;
                run)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--port=[App port]:port:' \
                        '--stop[Stop running process]' \
                        '--no-sandbox[Let the process write outside the workspace]' \
                        '--help[Show help]'
                    ;;
                switch)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--help[Show help]'
                    ;;
                ls)
                    _arguments \
                        '--json[Print the workspaces as JSON]' \
                        '--help[Show help]'
                    ;;
                diff)
                    _arguments \
                        '*:workspace:_forkspace_workspaces' \
                        '--json[Print the changes as JSON]' \
                        '--help[Show help]'
                    ;;
                merge)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--undo[Put the base back as it was before the latest merge]' \
                        '--list[With --undo, list the merges that can be undone]' \
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
                checkpoint)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '-m=[Label]:label:' \
                        '--message=[Label]:label:' \
                        '--all[Also save ignored paths]' \
                        '--help[Show help]'
                    ;;
                checkpoints)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--json[Output as JSON]' \
                        '--help[Show help]'
                    ;;
                gc)
                    _arguments \
                        '--older-than=[Only remove unchanged workspaces older than this]:age:' \
                        '--yes[Remove without asking]' \
                        '--help[Show help]'
                    ;;
                limit)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--memory=[Memory limit or none]:size:' \
                        '--cpu=[CPU limit or none]:percent:' \
                        '--help[Show help]'
                    ;;
                restore)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '2:checkpoint id:' \
                        '--resume[Finish an interrupted restore]' \
                        '--no-install[Skip dependency installation]' \
                        '--help[Show help]'
                    ;;
                upgrade|update)
                    _arguments \
                        '1:version:' \
                        '--check[Only report whether a newer release exists]' \
                        '--help[Show help]'
                    ;;
                skill)
                    _arguments '--help[Show help]'
                    ;;
                hook)
                    _arguments \
                        '1:event:(worktree-create worktree-remove claude-settings)' \
                        '--discard[Drop the workspace even when it has unmerged work]' \
                        '--help[Show help]'
                    ;;
            esac
            ;;
    esac
}

_forkspace "$@"
