# Changelog

Notable changes to forkspace. Versions follow [Semantic Versioning](https://semver.org/): before
1.0, a minor version (0.x.0) may change commands or behaviour, and a patch version (0.x.y) only
fixes. Write the notes for the next release under `## Unreleased`; `scripts/release.sh` turns
that heading into the version and date, and the section becomes the GitHub release notes.

## 0.2.0 - 2026-11-17

Upgrading: workspaces made by 0.1.0 keep working. Workspaces created, pulled or merged by this
version use a binary manifest and `pull` deltas that 0.1.0 cannot read, so drop them before
going back to 0.1.0. `forkspace enter -- command` now exits with the command's status, and
`merge`/`pull`/`merge --undo` run the package manager only when dependency files change.

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
- Stopping a dev server no longer always waits 200 ms after SIGTERM, `drop --kill` scans
  processes once for every workspace, and `run`/`switch` look for the server's port among
  its own processes instead of every process on the machine (checking every 10 ms).
- A workspace stacked on an overlay workspace is frozen from its parent's layers: only
  directories the parent changed are read through FUSE; the rest comes from the parent's
  frozen-copy manifest.
- Change scans of copy, reflink and APFS-clone workspaces (`diff`, `ls`, `gc`, `merge`)
  look rows up in the sorted manifest and skip `node_modules` as one range.
- `--copy` and reflink forks record their manifest while copying instead of walking the copy.
- `land` and `new --branch` read and write Git refs directly where the repository allows
  instead of starting `git` for each step, and `--branch` is saved with the workspace.
- Manifests carry a checksum, so opening one no longer parses every row; `ls` adds a new
  frozen copy to its cached size instead of measuring every copy again.
- `enter -- command` exits with the command's status (it always exited 0), as documented, so
  `forkspace enter --ephemeral -- make test` fails when the tests do.
- The `localhost:4000` proxy exits a few seconds after the last dev server stops, or when its
  `FORKSPACE_HOME` is gone, instead of holding the port until the next reboot.
- `pull` records only what changed in the project since the workspace's frozen copy instead
  of building a whole new copy (160 -> 65 ms), and copies create and finish directories on
  several threads.
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
