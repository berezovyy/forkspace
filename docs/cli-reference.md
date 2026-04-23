# CLI reference

The authoritative command and flag matrix is
[`workspace-contract.md`](workspace-contract.md#command-and-flag-matrix). Run
`forkspace --help` or `forkspace <command> --help` for the version-specific syntax.

Commands described in future phase files are not available until their phase lands. The
retired `nuke` command and the unimplemented `path`, `up`, `down`, `snap`, `log`, `blame`,
`gc`, `serve`, `sleep`, `wake`, `tree`, `collapse`, and `race` commands are not part of the
CLI. Likewise, `fork -n`, `--memory`, `--cpu`, and `--ttl` are not implemented flags.
### `forkspace pull [name]`

Bring base changes into a workspace with a three-way reconcile and then update its ancestry.
The name defaults to `$FORKSPACE_WORKSPACE`. Use `--dry-run` (optionally `--json`) to inspect the
plan, `--theirs` to resolve conflicts with the base version, `--resume` after interruption,
and `--no-install` to skip dependency installation.

### `forkspace fork --branch <branch>`

Preflight a normal Git checkout, fork it, and create an independent topic branch in the
workspace. Invalid or existing branches fail before the filesystem fork.

`--copy` consents to the portable full-directory-copy backend. It can be forced on every
platform and is slower and disk-consuming by design; without it, Linux automatically falls
through from overlay to reflink to copy while macOS diagnoses non-APFS placement.

### `forkspace land <name>`

Fetch a workspace's committed branch into the root base repository without network access or
history rewriting. `--as <branch>` chooses a different target, `--force` replaces divergent
target history, and `--drop` removes the workspace only after the ref update succeeds.
