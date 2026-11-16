# Changelog

Notable changes to forkspace. Versions follow [Semantic Versioning](https://semver.org/): before
1.0, a minor version (0.x.0) may change commands or behaviour, and a patch version (0.x.y) only
fixes. Write the notes for the next release under `## Unreleased`; `scripts/release.sh` turns
that heading into the version and date, and the section becomes the GitHub release notes.

## Unreleased

- `merge`, `pull` and `merge --undo` run the package manager only when they change
  `package.json`, a lockfile or package-manager settings. Before, every merge into a
  JavaScript project ran `npm install` (37 s for a one-line edit in a Next.js app); `restore`
  now also installs when only `package.json` changed.
- Forks of a project that has not changed copy the fork manifest the first fork recorded
  instead of walking the frozen copy again, and skip the scratch fuse-overlayfs mount once a
  probe on this machine has succeeded: about half the time of a Linux overlay fork.
- The first fork of a project, and `--copy` and reflink forks, copy files on several threads.
- `drop` of several workspaces looks for processes using them with one scan instead of two per
  workspace, spread over several threads, and stops all their systemd slices with one
  `systemctl` call (none for a workspace that has no slice).
- The first fork of a project no longer waits for its frozen copy to reach the disk: the copy
  is flushed in the background, and after a crash it is checked against its manifest before
  it is used again. Files are copied with `copy_file_range`, and the tree is read on several
  threads, each entry looked up relative to its open directory.
- Fork manifests are a binary, sorted file, so `merge` reads only the rows it needs (planning
  went from 20 ms to 2 ms on a Next.js app). JSON-lines manifests from 0.1.0 are still read.
- `new` reads the workspace's Git `HEAD` without starting `git`, and scans for Python virtual
  environments in the frozen copy rather than through the FUSE view.
- Measured on ext4 (NVMe) with a 424 MB `node_modules`: first fork 1.15 s → 0.26 s, later
  forks 78 → 25 ms, a one-file merge 3.7 s → 21 ms, `pull` 4.2 s → 0.19 s, `drop` of two
  workspaces 99 → 21 ms.

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
