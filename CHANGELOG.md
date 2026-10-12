# Changelog

Notable changes to forkspace. Versions follow [Semantic Versioning](https://semver.org/): before
1.0, a minor version (0.x.0) may change commands or behaviour, and a patch version (0.x.y) only
fixes. Write the notes for the next release under `## Unreleased`; `scripts/release.sh` turns
that heading into the version and date, and the section becomes the GitHub release notes.

## Unreleased

The first release.

- `forkspace new` forks a project directory into a workspace, including the files Git
  ignores (`node_modules`, build caches, `.env`), with the fastest backend the host offers:
  fuse-overlayfs or reflinks on Linux, APFS clones on macOS, or a plain copy (`--copy`).
- `enter`, `diff`, `compare`, `merge` (with `--undo`), `pull`, `land` and `drop` to work in a
  workspace, review it, and bring its changes or commits back.
- `checkpoint`, `checkpoints` and `restore` save and return to states inside a workspace.
- `run` and `switch` give each workspace its own dev server behind one `localhost:4000` proxy.
- `forkspace upgrade` moves to a newer release; prebuilt binaries for Linux and macOS
  (x86_64 and arm64), or `cargo install forkspace`.
