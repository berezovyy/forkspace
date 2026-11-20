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
`state/migrate.rs` brings forward. A command that changes nothing does not rewrite it, and
loading parses it once, straight into the records; with 1,000 workspaces the file is about
700 KB and every command still reads it whole under the one lock (see
[platform-parity.md](platform-parity.md#many-workspaces-on-linux)). Fork has an intent/commit boundary. Merge, pull and restore
apply atomic per-path steps through resumable journals; every step is safe to apply twice, so
progress is saved at most every 100 ms rather than after each step. Every state load sweeps
routine leftovers (`state/recovery.rs`); an unfinished operation needs its explicit `--resume`.
Process records include a per-spawn identity, so recycled PIDs are never signalled.

A new frozen copy is not flushed before the fork returns: it records the boot it was written
in, and a detached `sync -f` marks it `durable` once the data is on disk. The flush starts
after the command's own small `fsync`s, so they do not queue behind it. Until then the copy
is trusted only within the same boot; after a crash, state recovery checks every copy a
workspace uses against its manifest before marking it durable, and a copy that does not match
is marked damaged, reported once, and never reused.

Fork manifests are binary: a header, one offset per row, and rows sorted by encoded path, so
a merge looks up the rows it needs by binary search instead of parsing every row. A frozen
copy carries the manifest of its tree, written while it is built; forks of it install that
file as a hard link (manifests are only ever replaced by rename), and the next freeze reads
it to decide which files to link and whether the project changed. Older JSON-lines manifests
are still read. A manifest carries a checksum of its rows, checked when it is opened.

`pull` re-baselines a sealed overlay on a delta rather than a new full copy: the entries that
changed since the full frozen copy the workspace builds on (with the directories above them),
a list of deleted paths, and the manifest of the whole project at pull time. A path's
fork-time version comes from the delta, else from the full copy unless the delta lists it as
deleted (`Workspace::ancestor_file`). A delta is never mounted or reused for a new fork;
the full copy below it stays alive while a workspace measures against the delta.

A workspace stacked on an overlay workspace freezes from the parent's layers rather than its
mounted view: directories the parent's upper layer has are read through the view, so
fuse-overlayfs decides what they show, and every other directory is the parent's frozen copy,
taken from its manifest and hard-linked without being read.

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
├── workspace/             the Workspace record; create, remove, pre-command guards, private .git,
│                          the background Git index refresh after a fork
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
├── exclude.rs             what forks leave out: nested worktrees, .forkspaceexclude
├── git.rs                 git subprocess helpers
├── paths.rs               every location under FORKSPACE_HOME
├── util/                  fs helpers, parallel and bulk directory walks, glob
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
