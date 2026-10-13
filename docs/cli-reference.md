# CLI reference

The authoritative command and flag matrix is
[`workspace-contract.md`](workspace-contract.md#command-and-flag-matrix). Run
`forkspace --help` or `forkspace <command> --help` for the version-specific syntax.

Commands described in future phase files are not available until their phase lands. The
retired `nuke` command and the unimplemented `path`, `up`, `down`, `snap`, `log`, `blame`,
`serve`, `sleep`, `wake`, `tree`, `collapse`, and `race` commands are not part of the CLI.
Likewise, `new -n` and `--ttl` are not implemented flags.

Commands entered through `forkspace enter` or started by `forkspace run` receive
`FORKSPACE_WORKSPACE`, `FORKSPACE_BASE`, `FORKSPACE_VIEW`, and (unless already set)
`COMPOSE_PROJECT_NAME`. `FORKSPACE_VIEW` is always the application-facing directory regardless of
the recorded backend; Linux overlay workspaces additionally expose `FORKSPACE_UPPER`.

### `forkspace run <name> -- <command>`

Start a dev server in the workspace with `$PORT` set, and point the `localhost:4000` proxy at
it. `run` then waits up to `--port-wait` seconds (default 10, `0` to skip) for the server to
listen; a server that ignored `$PORT` is routed to the port it actually opened.

### `forkspace pull [name]`

Bring base changes into a workspace with a three-way reconcile and then update its ancestry.
The name defaults to `$FORKSPACE_WORKSPACE`. Use `--dry-run` (optionally `--json`) to inspect the
plan, `--theirs` to resolve conflicts with the base version, `--resume` after interruption,
and `--no-install` to skip dependency installation.

### `forkspace new --branch <branch>`

Preflight a normal Git checkout, fork it, and create an independent topic branch in the
workspace. Invalid or existing branches fail before the filesystem fork.

`--copy` consents to the portable full-directory-copy backend. It can be forced on every
platform and is slower and disk-consuming by design; without it, Linux automatically falls
through from overlay to reflink to copy while macOS diagnoses non-APFS placement.

### `forkspace land <name>`

Fetch a workspace's committed branch into the root base repository without network access or
history rewriting. `--as <branch>` chooses a different target, `--force` replaces divergent
target history, and `--drop` removes the workspace only after the ref update succeeds.

### `forkspace compare <name> <name>...`

Summarize two or more same-origin attempts in argument order. The default view shows source,
dependency, and test counts plus contested/unique/partial path buckets. `--files` emits the
per-path A/M/D matrix and `--json` emits the same facts for tooling. It deliberately provides
no winner or quality score.

`forkspace diff --stat` includes non-empty review signals. `forkspace merge` prints its reviewed plan
before applying by default; `--quiet` suppresses that pre-apply record for scripts.

### `forkspace new --live`

Mount the base itself as the overlay's lower layer instead of a frozen copy (Linux overlay
only). What the workspace shows after the base changes is then undefined; `ls` marks such
workspaces `(live)`. Without `--live`, the first fork of a project freezes a copy of it under
`FORKSPACE_HOME/frozen` and later forks reuse or hard-link it.

### `forkspace merge --undo [name]`

Put the base back as it was before the newest merge (of `name`). Paths changed after that
merge, or by a newer merge, are left alone and listed. `--undo --list` shows the merges that
can be undone; undo data is kept for the last 10 merges or 14 days, whichever is more. Files
both sides changed are merged line by line; if the same lines changed, `merge` refuses and
`pull` writes conflict markers into the workspace instead.

### `forkspace checkpoint [name] [-m label] [--all]`, `checkpoints [name]`, `restore <name> <id>`

Save what changed since the fork (with `--all`, ignored paths such as `node_modules` too),
list the saved checkpoints, and return a workspace to one. Restore takes an automatic
checkpoint first, so it can be undone, and `restore <name> --resume` finishes an interrupted
restore. `pull` takes an automatic checkpoint before it rewrites files.

### `forkspace drop <name> --kill`

`drop` refuses while processes run in the workspace and lists them; `--kill` stops the ones
running there (processes that only have files open there are never killed). On Linux, `enter`
and `run` start commands in a systemd user slice per workspace
(`FORKSPACE_NO_SYSTEMD=1` turns that off), and `ls` shows how many processes run in each
workspace and their memory use.

### `forkspace new --memory <size> --cpu <percent>`, `forkspace limit <name>`

Linux: throttle a workspace above `--memory` (`MemoryHigh`) and cap its CPU time at
`--cpu` percent of one CPU (`CPUQuota`); `limit` shows or changes them, `none` removes one. All
workspaces together run under `forkspace.slice` with half the CPU weight of your own apps
(`FORKSPACE_CPU_WEIGHT`, default 50) and a shared memory ceiling (`FORKSPACE_MEMORY_HIGH`,
default 75%). On macOS, commands from `run` and `enter -- <command>` run at background priority.

### Write sandbox: `--no-sandbox`, `enter --sandbox-info`

Commands started by `enter` and `run` cannot write to the original project or to other
workspaces (Landlock on Linux, `sandbox-exec` on macOS). `enter <name> --sandbox-info` lists
where they may write; `--no-sandbox` or `FORKSPACE_SANDBOX=off` turns the sandbox off. Setuid
programs such as `sudo` gain no privileges inside it.

### `forkspace gc [--older-than 7d] [--yes]`

List what forkspace could give back (unchanged, idle workspaces older than `--older-than`;
unused frozen copies; undo data and checkpoints past retention; leftovers) and remove it after
confirmation, or at once with `--yes`. Workspaces with changes are listed and kept. `ls` shows
each workspace's private size and a total. Internal stores carry a `CACHEDIR.TAG` (and a Time
Machine exclusion on macOS); `FORKSPACE_BACKUP_WORKSPACES=off` excludes workspaces as well.

### `forkspace upgrade [version]`

Replace the running binary with the latest GitHub release, or with `version` (newer or older).
The download is checked against the release's SHA-256 checksum and moved into place
atomically. A binary installed by `cargo install` is upgraded with
`cargo install forkspace --version <version>`, and so is any platform the release has no
prebuilt binary for, when Cargo is on `PATH`. A Homebrew install is left to `brew upgrade`.
`--check` only reports whether a newer release exists. It needs `curl` and `tar`; workspaces
and their state carry over, since state written by an older version is migrated when it is
next read.

### `FORKSPACE_TRACE=1`

Any command run with `FORKSPACE_TRACE=1` prints one line per timed phase to stderr, for
example `forkspace-trace: materialize 41.203ms`. The phases are `preflight`, `materialize`,
`manifest`, and `state` for `new`; `scan`, `scan-base`, `plan`, `apply`, and `install` for
`diff`, `ls`, `merge`, and `pull`; `destroy` for `drop`; and `command` for the whole command.
Stdout is unchanged. [`scripts/bench.py`](../scripts/bench.py) records these phases next to
each wall-clock time.
