# Architecture

Forkspace keeps the CLI and state model backend-neutral. `WorkspaceBackend` owns preflight,
materialization, view lookup, change enumeration, destruction, capabilities, and shell
variables. The recorded `backend_kind` is sticky for the workspace lifetime.

## Backends

- `linux`: a fuse-overlayfs lower/upper/work/merged view. The upper is the fast change source.
- `mac-clone`: one recursive APFS `clonefile` into an ordinary directory.
- `linux-reflink`: an O(entries) walk with per-file `FICLONE`, falling back per file.
- `copy`: the unconditional O(bytes) directory-copy floor.

Directory snapshot backends share the fork manifest, ctime scanner, plan-first merge, pull
re-baseline, stacking, and trash-rename lifecycle. Selection probes capabilities only for a
new fork; commands on an existing workspace dispatch from its persisted kind.

## Durable control plane

`state.json` is written through fsync + rename under `flock`. Fork uses an intent/commit
boundary. Merge and pull apply atomic per-path steps through resumable journals. Routine
terminal residue is swept opportunistically; non-terminal operations require the explicit
`--resume` command. Process records include a per-spawn identity so recycled PIDs are never
killed from stale state.

## Review and exchange

`merge` transfers file state and flattens it. `land` locally fetches commit objects and
preserves history. `pull` reconciles the moving origin and installs a new ancestor manifest.
`compare` and review signals are pure fact extraction over the same backend changes used by
`diff`; they never score or choose an attempt.

## Source layout

Dependencies point downward: commands use the domain modules, which use the backend and
infrastructure layers. Nothing below `commands/` prints user-facing results.

```
src/
├── main.rs, cli.rs        entry point and clap definitions
├── commands/              one thin handler per subcommand: validate args, call the domain, print
├── workspace/             the Workspace record; create, remove, and pre-command guards
├── state/                 state.json store and flock (mod), v1 migration, crash recovery sweeps
├── backend/               WorkspaceBackend trait, kind selection, capability probes
│   ├── overlay/           fuse-overlayfs view: mount, whiteouts
│   └── snapshot/          directory snapshots via a ForkPrimitive: copy, reflink, clonefile
├── changes/               change detection: FileChange model, filter, fork manifest,
│                          overlay/snapshot/cumulative scans, rendering
├── sync/                  moving changes: merge plan, resumable journal, merge, pull, install
├── review/                read-only facts: dependency/test signals, multi-workspace compare
├── process/               PID identity, detached spawning, dev servers, proxy
├── git.rs                 git subprocess helpers
├── paths.rs               every location under FORKSPACE_HOME
├── util/                  fs helpers (atomic write, timestamps, byte compare), glob
└── error.rs               ForkspaceError and exit codes
```

Where to start for common changes:

- **New subcommand:** args in `cli.rs`, handler in `commands/`, logic in the matching domain module.
- **New backend:** implement `WorkspaceBackend`, or a `ForkPrimitive` for a directory snapshot,
  and register it in `backend/mod.rs`.
- **New on-disk location:** add it to `paths.rs`; recovery sweeps live in `state/recovery.rs`.
