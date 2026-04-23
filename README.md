<p align="center">
  <img src="assets/banner.svg" width="700" alt="forkspace" />
</p>

<p align="center">
  <strong>Instant, zero-copy, disposable workspaces powered by OverlayFS.</strong>
</p>

<p align="center">
  No Docker. No root. Nothing is actually copied. Only what you change gets stored. <br/>
Fork time depends on the project and storage backend; release benchmarks are not published yet.
</p>

<p align="center">
  <a href="https://crates.io/crates/forkspace"><img src="https://img.shields.io/crates/v/forkspace?style=flat-square&color=58a6ff" alt="crates.io" /></a>
  <a href="https://github.com/berezovyy/forkspace/blob/master/LICENSE"><img src="https://img.shields.io/badge/license-MIT-a371f7?style=flat-square" alt="license" /></a>
  <a href="#requirements"><img src="https://img.shields.io/badge/Linux%20%7C%20macOS-supported-238636?style=flat-square" alt="Linux and macOS" /></a>
  <a href="#install"><img src="https://img.shields.io/badge/no_root-required-238636?style=flat-square" alt="no root" /></a>
  <a href="#how-it-works"><img src="https://img.shields.io/badge/fork_time-unbenchmarked-58a6ff?style=flat-square" alt="fork time unbenchmarked" /></a>
</p>

<br/>

## Why forkspace?

`cp -r` is slow. Docker is overkill. Git worktrees can carry over files via `.worktreeinclude` or a `post_setup_worktree` hook — but for build caches (`.next/`, `target/`, `dist/`) you're stuck picking: copy them (slow, gigabytes) or symlink them (fast, but both worktrees write to the same cache and race). Forkspace's overlay gives you both: the original is inherited read-only, writes diverge into a per-workspace upper layer. Instant *and* isolated, with zero config.

|                         |  `cp -r`  |  Docker   | Git Worktree |   **forkspace**   |
| :---------------------- | :-------: | :-------: | :----------: | :-----------: |
| **Speed**               |   Slow    |   Slow    |     Fast     | **Unbenchmarked** |
| `.env` / `node_modules` |    Yes    |    No     |      No      |    **Yes**    |
| **Isolation**           |   Full    |   Full    |   Partial    |   **Full**    |
| **Disk usage**          | Full copy | Full copy |   Partial    | **Zero-copy** |
| **Root required**       |    No     |   Often   |      No      |    **No**     |
| **Cleanup**             |  Manual   |  Manual   |    Manual    | `forkspace drop`  |

<br/>

## Quick start

```bash
forkspace fork . --name experiment     # instant - nothing is copied
forkspace enter experiment             # opens a shell inside the workspace

# make changes, break things, go wild
# ...then exit the shell

forkspace diff experiment              # see what changed
forkspace diff experiment --full       # unified diff

forkspace merge experiment --drop      # happy? merge back
forkspace drop experiment              # not happy? throw it away
```

```
project/                 forkspace fork . --name experiment
├── src/
├── node_modules/  ───►  ~/.forkspace/experiment/merged/    (looks identical)
├── .env                   ├── src/                      (read from original)
└── package.json           ├── node_modules/             (read from original)
                           ├── .env                      (read from original)
                           └── src/api.rs                ← only changed file stored
```

<br/>

## Install

```bash
cargo install --path .
```

<details><summary><strong>Prerequisites</strong> - fuse-overlayfs &amp; fuse3</summary>

```bash
# Ubuntu/Debian
sudo apt install fuse-overlayfs fuse3

# Fedora
sudo dnf install fuse-overlayfs fuse3

# Arch
sudo pacman -S fuse-overlayfs fuse3
```

</details>

<br/>

## Workflows

### Parallel experiments

```bash
forkspace fork . --name approach-a
forkspace fork . --name approach-b

# two isolated workspaces, same base, zero copying
# open each in a separate terminal, editor, or agent

forkspace diff approach-a --stat
forkspace diff approach-b --stat

forkspace merge approach-a --commit -m "switch to Hono"
forkspace drop approach-b
```

<details><summary><strong>Dev server hot-swap</strong></summary>

Run dev servers in different workspaces and switch between them without restarting anything:

```bash
forkspace fork . --name feature-a
forkspace fork . --name feature-b

forkspace run feature-a --port 3000 -- npm run dev
forkspace run feature-b --port 3000 -- npm run dev

# proxy on localhost:4000 routes to the active workspace
forkspace switch feature-a    # localhost:4000 → feature-a
forkspace switch feature-b    # localhost:4000 → feature-b
```

</details>

<details><summary><strong>Stacking (fork-of-fork)</strong></summary>

```bash
forkspace fork . --name step-1
# ... make data model changes in step-1 ...

forkspace fork step-1 --name step-2
# ... build API on top of step-1's changes ...

# see everything that changed from root to step-2
forkspace diff step-2 --cumulative
```

</details>

<details><summary><strong>Ephemeral workspaces</strong></summary>

```bash
# auto-creates a workspace, opens a shell, destroys on exit
forkspace enter --ephemeral

# or run a single command
forkspace enter --ephemeral -- make test

# `--tmpfs` is reserved but not implemented yet; use disk-backed ephemeral workspaces.
```

</details>

<br/>

## Commands

| Command  | Description                                                            |
| :------- | :--------------------------------------------------------------------- |
| `fork`   | Create a workspace from a directory                                    |
| `enter`  | Shell into a workspace (`--ephemeral`, `--merge-on-exit`, `--create`)  |
| `diff`   | Show changes (`--stat`, `--full`, `--files`, `--json`, `--cumulative`) |
| `merge`  | Apply changes to base (`--commit`, `--patch`, `--drop`)                |
| `pull`   | Bring base changes into a workspace and re-baseline it                |
| `land`   | Preserve a workspace's committed topic-branch history in the base     |
| `compare`| Compare parallel attempts on source, dependency, test, and path facts |
| `drop`   | Remove a workspace (`--force`, `--all`, `--glob`)                      |
| `ls`     | List workspaces                                                        |
| `run`    | Start a dev server with automatic proxy setup                          |
| `switch` | Hot-swap which workspace the proxy routes to                           |

<br/>

## How it works

Forkspace uses [fuse-overlayfs](https://github.com/containers/fuse-overlayfs) to create a union mount:

```
merged/    ← you work here (reads from both layers)
  │
  ├── upper/   ← your changes live here (copy-on-write)
  └── lower/   ← original project (read-only, untouched)
```

When you read a file, it comes from the original. When you write, the file is copied to upper first (copy-on-write), then modified there. The original never changes. Deletes are tracked as whiteout markers. The upper directory _is_ the diff - no diffing algorithm needed.

Forking avoids copying project contents on the Linux overlay backend. Merging moves files from upper back to the base. Dropping unmounts and deletes the upper.

<br/>

## Under the hood

> **No daemon. No runtime. Just syscalls and a state file.**

| Primitive           | Role                                                              |
| :------------------ | :---------------------------------------------------------------- |
| **OverlayFS**       | Copy-on-write file isolation - the kernel does the hard work      |
| **flock()**         | File-level locking so parallel forkspace commands don't corrupt state |
| **setsid()**        | Dev servers run as independent sessions, survive your terminal    |
| **kill() + pgid**   | Stopping a server kills the entire process tree                   |
| **Atomic writes**   | write → fsync → rename - state file can't corrupt on crash        |
| **User namespaces** | Unprivileged operation - no root needed, ever                     |

<br/>

## Requirements

- Linux or macOS; no root/sudo required
- Fastest on Linux with `fuse-overlayfs`, or on macOS with APFS clones
- Linux falls back to per-file reflinks and then a portable full copy
- `--copy` explicitly exercises the universal fallback on either platform

## License

MIT

<p align="center">
  <a href="https://github.com/berezovyy/forkspace/issues">Report Bug</a> &middot;
  <a href="https://github.com/berezovyy/forkspace/issues">Request Feature</a>
</p>
