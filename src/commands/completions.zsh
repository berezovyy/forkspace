#compdef forkspace

_forkspace_workspaces() {
    local -a names
    names=(${(f)"$(forkspace ls --json 2>/dev/null | sed -n 's/^    "name": "\(.*\)",$/\1/p')"})
    _describe 'workspace' names
}

_forkspace() {
    local -a commands
    commands=(
        'new:Copy the project you are in into a new workspace'
        'drop:Throw workspaces away (and stop what runs in them)'
        'enter:Open a shell (or run a command) inside a workspace'
        'run:Start a dev server in a workspace'
        'switch:Point localhost:4000 at another workspace'"'"'s dev server'
        'ls:List your workspaces: what changed, how old, what runs'
        'diff:Show what changed in a workspace, or compare several'
        'merge:Put a workspace'"'"'s changes into your project'
        'undo:Put your project back as it was before the latest merge'
        'pull:Bring your project'"'"'s newer changes into a workspace'
        'save:Save a snapshot of a workspace to go back to'
        'restore:Go back to a snapshot (without one, list them)'
        'setup:Set up the coding agents on this machine to use forkspace'
        'upgrade:Upgrade forkspace to the latest release'
        'update:Upgrade forkspace to the latest release'
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
                restore)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '2:snapshot id:' \
                        '--help[Show help]'
                    ;;
                upgrade|update)
                    _arguments \
                        '1:version:' \
                        '--help[Show help]'
                    ;;
            esac
            ;;
    esac
}

_forkspace "$@"
