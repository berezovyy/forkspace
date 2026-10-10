#compdef forkspace

_forkspace_workspaces() {
    local -a names
    names=(${(f)"$(forkspace ls --names 2>/dev/null)"})
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
        'compare:Compare parallel workspaces side by side'
        'merge:Apply workspace changes back to the base directory'
        'pull:Bring base changes into a workspace'
        'land:Preserve workspace commits in the base repository'
        'checkpoint:Save the current state of a workspace'
        'checkpoints:List a workspace'"'"'s checkpoints'
        'restore:Return a workspace to one of its checkpoints'
        'limit:Show or change a workspace'"'"'s memory and CPU limits'
        'gc:Show what disk forkspace could give back, and reclaim it'
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
                        '--from=[Directory or workspace to start from]:source:_files -/' \
                        '--session=[Session identifier]:session:' \
                        '--tmpfs[Keep changes in RAM]' \
                        '--branch=[Create a Git branch]:branch:' \
                        '--copy[Consent to a full directory copy]' \
                        '--live[Show later source changes (no frozen copy)]' \
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
                        '--create[Create if not exists]' \
                        '--from=[Base directory]:directory:_directories' \
                        '--ephemeral[Destroy on exit]' \
                        '--tmpfs[Keep changes in RAM]' \
                        '--copy[Consent to a full directory copy]' \
                        '--live[Show later base changes (no frozen copy)]' \
                        '--no-sandbox[Let the command write outside the workspace]' \
                        '--sandbox-info[List where commands may write, then exit]' \
                        '--merge-on-exit[Merge changes on exit]' \
                        '--session=[Session identifier]:session:' \
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
                land)
                    _arguments \
                        '1:workspace:_forkspace_workspaces' \
                        '--force[Replace a divergent branch]' \
                        '--drop[Drop after a successful land]' \
                        '--as=[Land under another branch]:branch:' \
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
            esac
            ;;
    esac
}

_forkspace "$@"
