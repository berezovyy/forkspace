# Changelog

Notable changes to forkspace. Versions follow [Semantic Versioning](https://semver.org/): before
1.0, a minor version (0.x.0) may change commands or behaviour, and a patch version (0.x.y) only
fixes. Write the notes for the next release under `## Unreleased`; `scripts/release.sh` turns
that heading into the version and date, and the section becomes the GitHub release notes.

## 0.1.0 - 2026-10-19

The first release.

- `forkspace new` forks a project directory into a workspace, including the files Git
  ignores (`node_modules`, build caches, `.env`), with the fastest backend the host offers:
  fuse-overlayfs or reflinks on Linux, APFS clones on macOS, or a plain copy (`--copy`).
- Workspaces are sealed: later changes to the project, including branch switches and commits,
  reach a workspace only through `forkspace pull`. Linux overlays mount a frozen copy of the
  project that later forks reuse or hard-link.
- `enter`, `diff`, `compare`, `merge`, `pull`, `land` and `drop` to work in a workspace, review
  it, and bring its changes or commits back. Files both sides changed merge line by line, and
  `merge --undo` puts the project back as it was.
- `checkpoint`, `checkpoints` and `restore` save and return to states inside a workspace.
- `run` and `switch` give each workspace its own dev server behind one `localhost:4000` proxy.
- Processes started in a workspace are tracked, so `drop` refuses while they run and
  `drop --kill` stops exactly them. On Linux each workspace can be limited (`--memory`,
  `--cpu`, `forkspace limit`), and all of them together stay below your own programs.
- Commands in a workspace cannot write to the original project or to other workspaces
  (Landlock on Linux, Seatbelt on macOS); `--no-sandbox` turns that off.
- `ls` shows each workspace's disk use, and `forkspace gc` gives back what is no longer needed.
- Scans skip folders whose changes never transfer, such as `node_modules`, and walk in parallel.
- `FORKSPACE_TRACE=1` prints where a command's time went; `scripts/bench.py` measures every
  backend.
- `forkspace upgrade` moves to another release; prebuilt binaries for Linux and macOS
  (x86_64 and arm64), or `cargo install forkspace`.
