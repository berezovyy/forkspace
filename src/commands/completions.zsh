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
        'undo:Put your project back as it was before the latest merge'
        'pull:Bring base changes into a workspace'
        'save:Save a snapshot of a workspace to go back to'
        'restore:Go back to a snapshot (without one, list them)'
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
                        '--help[Show help]'
                    ;;
                drop)
                    _arguments \
                        '*:workspace:_forkspace_workspaces' \
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
                        '--help[Show help]'
                    ;;
                undo)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--help[Show help]'
                    ;;
                pull)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--json[Print the pull plan as JSON]' \
                        '--help[Show help]'
                    ;;
                save)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '2:note:' \
                        '--help[Show help]'
                    ;;
                gc)
                    _arguments \
                        '--older-than=[Only remove unchanged workspaces older than this]:age:' \
                        '--yes[Remove without asking]' \
                        '--help[Show help]'
                    ;;
                restore)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '2:snapshot id:' \
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
