# CLI reference

Every command, its flags, the environment it reads and sets, and its exit codes. `forkspace
--help` and `forkspace <command> --help` show the syntax of the installed version.

Flags marked *Linux* need a Linux backend or service. Elsewhere `--live` and `--tmpfs` fail
with exit code 6 and a remedy; `--memory` and `--cpu` are recorded with a warning and have no
effect without a systemd user session.

## Create and enter

### `forkspace new [name] [--from <source>]`

Fork `source` (default: the current directory), or another workspace, into a workspace named
`name` (default: the source directory's name). The backend is picked by probing: fuse-overlayfs,
then reflinks, then a copy on Linux; APFS clones on macOS. The choice is recorded and never
changes for that workspace.

| Flag | |
| :-- | :-- |
| `--branch <branch>` | Create a Git branch in the workspace and record it for `land`. Invalid or existing branches fail before anything is forked. |
| `--copy` | Agree to a full directory copy when nothing faster works. Without it, macOS refuses a project that is not on APFS; Linux falls through to a copy on its own and says so. |
| `--live` | *Linux.* Mount the project itself instead of a frozen copy. Faster to create, but the workspace shows later project changes, with undefined results. `ls` marks it `(live)`. |
| `--tmpfs` | *Linux.* Keep the workspace's changes on an existing tmpfs (`$XDG_RUNTIME_DIR` or `/dev/shm`). Lost on reboot. |
| `--memory <size>` | *Linux.* Throttle the workspace's processes above this much memory, e.g. `4G` or `75%`. |
| `--cpu <percent>` | *Linux.* Cap the workspace's CPU time, as a percentage of one CPU, e.g. `200%`. |
| `--session <id>` | Record a session identifier, to group related workspaces. |

### `forkspace enter [name] [-- command…]`

Open a shell in the workspace, or run `command` there and exit with its status. Without a
name, `enter` uses the workspace the current directory belongs to.

| Flag | |
| :-- | :-- |
| `--create` (`--new`) | Create the workspace first if it does not exist; `--from`, `--copy`, `--live` and `--tmpfs` then apply as for `new`. |
| `--ephemeral` | Drop the workspace when the shell or command exits. |
| `--merge-on-exit` | Merge the workspace's changes into its base when the shell exits. |
| `--no-sandbox` | Let the shell or command write outside the workspace (see [Sandbox](#sandbox)). |
| `--sandbox-info` | List where commands in the workspace may write, then exit. |
| `--session <id>` | Set `FORKSPACE_SESSION` in the shell. |

## Review and bring changes back

### `forkspace ls`

List workspaces with their backend, changed files, private disk size, processes and memory,
and a total. `--names` prints names only, `--json` the full records (including the backend
probe's reason). `--no-gitignore` counts ignored paths too.

### `forkspace diff <name>`

Show what changed since the fork: a list of files by default, `--stat` for a summary with
[review signals](reviewing-changes.md), `--full` for a unified diff, `--files` for paths only,
`--json` for tools. `--cumulative` compares a stacked workspace with the root project instead
of its parent. `--no-gitignore` includes ignored paths.

### `forkspace compare <name> <name>…`

Compare two or more workspaces forked from the same place, in the order given: source,
dependency and test changes per workspace, and which paths all, some, or one of them touched.
`--files` prints the per-path matrix, `--json` the same facts for tools. It never picks a
winner.

### `forkspace merge <name>`

Apply the workspace's changes to its base, after printing the plan. Files both sides changed
are merged line by line; when the same lines changed, merge refuses and changes nothing. A
lockfile change runs the project's package manager afterwards.

| Flag | |
| :-- | :-- |
| `--dry-run` | Print the plan and stop. |
| `--commit`, `-m <message>` | Commit the merged files in the base's Git repository (`-m` implies `--commit`). |
| `--patch` | Print a unified patch instead of merging. |
| `--drop` | Drop the workspace after a successful merge. |
| `--no-install` | Skip the package-manager step. |
| `--overwrite-base` | Take the workspace's version where the base changed too. |
| `--quiet` | Do not print the plan before applying it. |
| `--resume` | Finish an interrupted merge. |
| `--undo [name]` | Put the base back as it was before the latest merge (of `name`). Paths changed since then are left alone and listed. Undo data is kept for the last 10 merges or 14 days, whichever is more. |
| `--undo --list` | List the merges that can be undone. |

### `forkspace pull [name]`

Bring project changes made since the fork into the workspace, and move its fork point to now.
Files both sides changed are merged line by line, with conflict markers where the same lines
changed; binary and very large files both sides changed refuse the pull. A checkpoint is taken
first. The name defaults to `$FORKSPACE_WORKSPACE`.

`--dry-run` (with `--json`) prints the plan, `--theirs` takes the project's version on
conflicts, `--resume` finishes an interrupted pull, `--no-install` skips the package manager.

### `forkspace land <name>`

Bring the commits of the workspace's Git branch into the project's repository, without
network access and without rewriting them. The working tree of the project is not touched.
`--as <branch>` lands under another branch name, `--force` replaces a branch that has
diverged, `--drop` drops the workspace once the branch is updated.

## Inside a workspace

### `forkspace checkpoint [name] [-m <label>] [--all]`

Save what changed since the fork. `--all` also saves ignored paths such as `node_modules`. The
newest 20 manual and 10 automatic checkpoints are kept. The name defaults to
`$FORKSPACE_WORKSPACE`.

### `forkspace checkpoints [name]`

List a workspace's checkpoints; `--json` for tools.

### `forkspace restore <name> <id>`

Return the workspace to a checkpoint. A checkpoint of the current state is taken first, so a
restore can be undone. `--resume` finishes an interrupted restore; `--no-install` skips the
package manager when a lockfile changes.

### `forkspace run <name> -- <command>`

Start a dev server (or any long-running command) in the workspace with `$PORT` set, and point
the proxy on `localhost:4000` at it. `run` waits up to `--port-wait` seconds (default 10, `0`
to skip) for the server to listen; a server that ignores `$PORT` is routed to the port it
opened. `--port` is the port the app is told to use (default 3000), `--stop` stops the
workspace's process, `--no-sandbox` lets it write outside the workspace.

### `forkspace switch <name>`

Point the `localhost:4000` proxy at another workspace's dev server. New connections go to the
new server; open ones finish where they are.

### `forkspace limit <name> [--memory <size>] [--cpu <percent>]`

*Linux.* Show or change a workspace's limits; `none` removes one. Running processes get the
change at once. All workspaces together also run under one `forkspace.slice` with half the CPU
weight of your own programs and a shared memory ceiling (see `FORKSPACE_CPU_WEIGHT` and
`FORKSPACE_MEMORY_HIGH`). On macOS, `run` and `enter -- <command>` start commands at background
priority instead.

## Clean up

### `forkspace drop <name>`

Remove a workspace. `drop` refuses while processes run in it and lists them.

| Flag | |
| :-- | :-- |
| `--kill` | Stop the processes running in the workspace first. Processes that only have files open there are never killed. |
| `--force` | Remove a workspace that other workspaces were forked from, with them. |
| `--all` | Remove every workspace. |
| `--glob` | Treat `name` as a glob, e.g. `'attempt-*'`. |

### `forkspace gc [--older-than <age>] [--yes]`

List what forkspace could give back and remove it after asking, or at once with `--yes`:
unchanged, idle workspaces older than `--older-than` (default `7d`; units `s`, `m`, `h`, `d`,
`w`), frozen copies no workspace uses, undo data and checkpoints past retention, and leftovers
of interrupted commands. Workspaces with changes are listed and kept.

## Forkspace itself

### `forkspace upgrade [version]`

Replace the running binary with the latest release, or with `version` (newer or older). The
download is checked against the release's SHA-256 checksum and moved into place in one step.
A Cargo install is upgraded with `cargo install`, and so is a platform the release has no
binary for, when Cargo is on `PATH`; a Homebrew install is left to `brew upgrade`. `--check`
only reports whether a newer release exists. Needs `curl` and `tar`. Existing workspaces keep
working: state written by an older version is migrated when it is next read.

### `forkspace completions <shell>`

Print a completion script for `bash`, `zsh`, `fish`, `elvish` or `powershell`. The zsh script
completes workspace names: save it as `_forkspace` in a directory on `$fpath`. For bash,
add `source <(forkspace completions bash)` to `~/.bashrc`.

## Sandbox

Commands started by `enter` and `run` cannot write to the original project or to other
workspaces: Landlock on Linux, `sandbox-exec` on macOS. Setuid programs such as `sudo` gain no
privileges inside it. Temporary directories (`/tmp`, `$TMPDIR`, `$XDG_RUNTIME_DIR`) and the
dot-entries of your home directory, such as `~/.npm` and `~/.cargo`, stay writable.
`enter <name> --sandbox-info` lists where commands may write; `--no-sandbox` or
`FORKSPACE_SANDBOX=off` turns it off.

## Environment

Set by `enter` and `run` for the commands they start:

| Variable | |
| :-- | :-- |
| `FORKSPACE_WORKSPACE` | The workspace's name. Commands that take an optional name default to it. |
| `FORKSPACE_BASE` | The directory it was forked from. |
| `FORKSPACE_VIEW` | The workspace directory, on every backend. Use this one in scripts. |
| `FORKSPACE_UPPER` | Linux overlay only: the layer that holds the workspace's changes. |
| `FORKSPACE_SESSION` | The `--session` value, when given. |
| `COMPOSE_PROJECT_NAME` | `<project>-<workspace>`, unless already set, so Docker Compose keeps each workspace's containers apart. |
| `PORT` | `run` only: the port the app should listen on. |

Read by forkspace:

| Variable | |
| :-- | :-- |
| `FORKSPACE_HOME` | Where workspaces and state live (default `~/.forkspace`, kept owner-only). |
| `FORKSPACE_TRACE=1` | Print each command's timed phases to stderr, e.g. `forkspace-trace: materialize 41.203ms`. [`scripts/bench.py`](../scripts/bench.py) records them. |
| `FORKSPACE_SANDBOX=off` | Turn the write sandbox off by default. |
| `FORKSPACE_NO_SYSTEMD=1` | *Linux.* Do not start commands in a systemd user slice per workspace (no process tracking by slice, no limits). |
| `FORKSPACE_CPU_WEIGHT` | *Linux.* CPU weight of all workspaces together, against your programs' 100 (default 50). |
| `FORKSPACE_MEMORY_HIGH` | *Linux.* Memory ceiling of all workspaces together (default `75%`). |
| `FORKSPACE_BACKUP_WORKSPACES=off` | Mark workspaces as caches too, so backups skip them. Internal stores always carry a `CACHEDIR.TAG` (and a Time Machine exclusion on macOS). |
| `FORKSPACE_RELEASES_URL` | Where `upgrade` looks for releases (default: this repository's GitHub releases). |

For debugging and tests only: `FORKSPACE_BACKEND=<linux|mac-clone|linux-reflink|copy>` forces
a backend for new workspaces, `FORKSPACE_NO_LANDLOCK=1` and `FORKSPACE_NO_BULK=1` turn off
Landlock and macOS bulk directory reads.

## Exit codes

| Code | Meaning |
| --: | :-- |
| 0 | Success |
| 1 | An error with no more specific code |
| 2 | Command-line usage error |
| 3 | The workspace or a recorded path was not found |
| 4 | The workspace or branch already exists |
| 5 | Conflict or refusal: nothing was changed |
| 6 | The workspace's backend does not support this |
| 7 | The backend is unavailable here (for example, no FUSE) |

An environment or capability error is one line naming both the problem and what to do about
it, for the platform it happened on.
