# Architecture

Forkspace keeps the CLI and state model backend-neutral. `WorkspaceBackend` owns preflight,
materialization, view lookup, change enumeration, destruction, capabilities and shell
variables. The recorded `backend_kind` is sticky for the workspace's lifetime: selection probes
capabilities only for a new fork, and commands on an existing workspace dispatch from the
recorded kind.

## Backends

- `linux`: a fuse-overlayfs view whose lower layer is a frozen copy of the base. The upper
  layer is the change set, so diff reads it directly.
- `mac-clone`: one recursive APFS `clonefile` into an ordinary directory.
- `linux-reflink`: an O(entries) walk with a `FICLONE` per file, falling back to a byte copy
  per file.
- `copy`: the O(bytes) directory copy every platform can fall back to.

The three directory-snapshot backends share one `ForkPrimitive` seam and everything above it:
the fork manifest, the ctime scan that finds changes, the merge plan, pull, stacking, and drop by
renaming into trash before deleting in the background (one `rm` per trashed directory, holding a
lock so later sweeps leave it alone).

### macOS

The clone backend needs no daemon, filesystem extension, signing identity, kernel component or
Apple Developer Program membership. The project and `FORKSPACE_HOME/workspaces` must be on the
same APFS volume. Fork samples its timestamp before cloning, records an intent before touching
the filesystem, and commits state only once the view exists. A background worker (a re-executed
copy of the binary) writes the manifest: encoded raw paths, kind, size, nanosecond mtime, mode,
symlink target, and a dirty guard. The fork-time copy that line-level merges use is a second
clone of the project's visible tree, a whole directory per call, leaving ignored trees such as
`node_modules` out. `clonefile` gives every cloned directory a fresh mtime, so directories are
compared by kind and mode only, and a deleted directory by what it contains. APFS resolves
names case- and normalization-insensitively, so a rename between two spellings of one name
(`Foo.txt` to `foo.txt`) is applied as its deletion, then its addition. Dev servers run in the
clone directory, so FSEvents-based tools work normally. Processes are identified by their start
time from `proc_pidinfo`, because macOS hides the environment of Apple platform binaries such as
`/bin/sh`. The cost is O(entries), not O(1): directory metadata is materialized even though file
blocks are shared.

## Durable control plane

`state.json` is written with fsync and rename under `flock`, and carries a `version` that
`state/migrate.rs` brings forward. Fork has an intent/commit boundary. Merge, pull and restore
apply atomic per-path steps through resumable journals; every step is safe to apply twice, so
progress is saved at most every 100 ms rather than after each step. Every state load sweeps
routine leftovers (`state/recovery.rs`); an unfinished operation needs its explicit `--resume`.
Process records include a per-spawn identity, so recycled PIDs are never signalled.

## Review and exchange

`merge` transfers file state and flattens it; `land` fetches commit objects and keeps history;
`pull` reconciles the moving base and records a new fork point. Files both sides changed go
through a line-level three-way merge (`git merge-file`) against the fork-time version, which
`sync/ancestor.rs` keeps for snapshot backends. `compare` and the review signals are pure fact
extraction over the same change sets `diff` uses; they never score or choose an attempt.

## Source layout

Dependencies point downward: commands use the domain modules, which use the backend and
infrastructure layers. Nothing below `commands/` prints user-facing results.

```
src/
├── main.rs, cli.rs        entry point and clap definitions
├── commands/              one thin handler per subcommand: validate args, call the domain, print
├── workspace/             the Workspace record; create, remove, pre-command guards, private .git
├── state/                 state.json store and flock, migrations, crash-recovery sweeps
├── backend/               WorkspaceBackend trait, kind selection, capability probes
│   ├── overlay/           fuse-overlayfs: mounts, frozen copies, tmpfs layers, whiteouts
│   └── snapshot/          directory snapshots via a ForkPrimitive: copy, reflink, clonefile
├── changes/               change detection: FileChange model, filters, fork manifest and its
│                          worker, overlay/snapshot/cumulative scans, rendering
├── sync/                  moving changes: plans, journal, merge, pull, line merges, undo,
│                          checkpoints, package-manager installs
├── review/                read-only facts: dependency/test signals, multi-workspace compare
├── process/               spawning, per-workspace process tracking (systemd slices, /proc,
│                          libproc), limits, the write sandbox, dev servers, ports, proxy
├── disk.rs                disk use per workspace and what gc can reclaim
├── git.rs                 git subprocess helpers
├── paths.rs               every location under FORKSPACE_HOME
├── util/                  fs helpers, parallel and bulk directory walks, glob, trace timings
└── error.rs               ForkspaceError and exit codes
```

Where to start for common changes:

- **New subcommand:** args in `cli.rs`, handler in `commands/`, logic in the matching domain
  module; then the zsh completions (`commands/completions.zsh`), the help fixtures in
  `tests/fixtures/cli-contract/`, and the [CLI reference](cli-reference.md).
- **New backend:** implement `WorkspaceBackend`, or a `ForkPrimitive` for a directory snapshot,
  and register it in `backend/mod.rs`.
- **New on-disk location:** add it to `paths.rs`; recovery sweeps live in `state/recovery.rs`,
  and `disk.rs` should count it.
- **New state field:** bump the state `version` and add a step to `state/migrate.rs`.
