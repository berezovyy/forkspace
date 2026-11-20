# CLI reference

Every command, what it does, the environment it sets and reads, and its exit codes.
`forkspace --help` and `forkspace <command> --help` show the installed version's syntax.

Commands that take an optional workspace name use, without one, the workspace whose folder you
are in; outside any workspace, the only workspace there is. When neither applies, they list
the workspaces to choose from.

## Make and enter

### `forkspace new [name]`

Copy the project you are in into a new workspace and print its path. The project is the Git
repository you are in (its top folder), or else the folder you are in; inside a workspace it
is that workspace, so the new one is a workspace of a workspace. A folder Git ignores is
copied as a folder of its own. `new` refuses to copy your home folder or anything above it.

Without a name, the workspace is called after the project, `<project>-1`, `<project>-2`,
and so on, skipping names that a workspace or a branch of the project already has. In a Git
project, the workspace gets its own branch, named after the workspace.

How the copy is made is picked by itself: `fuse-overlayfs`, then file clones, then a plain
copy on Linux; APFS clones on macOS, or a plain copy when the project is on another disk. When
it is not the fastest way, `new` says why.

A workspace holds everything in the project folder, ignored files included, except:

- Git worktrees nested inside the project (such as Claude Code's `.claude/worktrees/*`). They
  are other checkouts, and a copy would still point at the project's repository, so a commit
  in the workspace would move the project's branches.
- `.claude/worktrees/`, whatever it holds.
- The paths the project's `.forkspaceexclude` lists, in `.gitignore` syntax. A `!` line can
  take back the `.claude/worktrees/` rule; nested worktrees always stay out.

Left-out paths never move between a workspace and its project: `merge` does not copy them
into the project, `pull` does not bring them into the workspace, and `diff` does not show
them.

A workspace starts with a Git index that matches its files, so the first `git status` in it
does not hash every tracked file again.

### `forkspace enter [name] [-- command…]`

Open a shell in the workspace, or run `command` there and exit with its status. A name that
has no workspace yet makes one from the project you are in, as `new` would; so does `enter`
with no name when there are no workspaces at all. Commands run in the
[sandbox](#sandbox).

## Look at changes

### `forkspace ls [--json]`

List your workspaces: name, what changed, how old, the disk it uses of its own, and what runs
in it; a `PROJECT` column appears when the workspaces come from more than one project. A last
line adds up what forkspace uses on disk. `--json` prints the full records for tools.

### `forkspace diff [names…] [--json]`

For one workspace: the changed files (`A` added, `M` modified, `D` deleted, with line counts),
notes about dependencies and tests that changed, and then a Git patch of the changes. Files a
patch cannot show (binary files, for example) are named on stderr.

For two or more workspaces made from the same project: a table of how the attempts differ
(source, dependency and test changes, and which paths all, some, or one of them touched).
It never picks one; see [Reviewing parallel changes](reviewing-changes.md).

`--json` prints the same facts for tools.

## Bring changes back

### `forkspace merge [name]`

Put the workspace's changes into the project it was made from, and say how many files were
added, changed and deleted. Files both sides changed are merged line by line, and `merge` says
which. When your project changed the same lines, or changed a file that cannot be merged
line by line, `merge` changes nothing and says to run `pull` first. When the merge changes
`package.json`, a lockfile or package-manager settings (such as `.npmrc`), the project's
package manager runs afterwards.

In a Git project, the merged files are left uncommitted in your project, for you to review
and commit. Commits made in the workspace are kept as the project branch named after the
workspace (`<name>-2` and so on when that branch has moved on, or is checked out); the
branch you have checked out never moves. The workspace stays until you drop it.

A merge that was interrupted (a crash, a full disk) is finished by the next `merge`.

### `forkspace undo [name]`

Put your project back as it was before the latest merge (of `name`, when given): changed
files go back to how they were, added files go away, and the branch that merge kept goes
back to where it was. Files changed again since the merge are left alone and listed. Undo is
kept for the last 10 merges or 14 days, whichever is more.

### `forkspace pull [name] [--json]`

Bring what changed in your project since the workspace was made (or last pulled) into the
workspace. Files both sides changed are merged line by line, with conflict markers where
the same lines changed; `merge` refuses until they are fixed. Where a file cannot be merged
line by line (binary or very large files, or a file one side deleted), the workspace takes
your project's version and `pull` says where to find yours: in the snapshot it saved before
pulling. The package manager runs when the pull changes `package.json`, a lockfile or
package-manager settings. An interrupted pull is finished by the next `pull`.

`--json` also prints what the pull does, as JSON on stdout for tools; its messages then go
to stderr.

## Save points

### `forkspace save [name] [note]`

Save a snapshot of what changed in the workspace, with an optional note, and print its
number. Changed files are kept compressed against how they looked when the workspace was
made, so a small edit of a large file costs a few hundred bytes. The newest 20 snapshots you
save, and the newest 10 that forkspace saves by itself (before `pull` and `restore`), are kept.

### `forkspace restore [name] [id]`

Without `id`, list the workspace's snapshots: number, age, files, size and note. With one,
put the workspace back to that snapshot. A snapshot of the current state is saved first and
named in the output, so a restore can be taken back. The package manager runs when the
restore changes `package.json`, a lockfile or package-manager settings.

## Dev servers

### `forkspace run [name] -- <command>`

Start a dev server (or any long-running command) in the workspace with `$PORT` set, and point
`localhost:4000` at it. `run` waits up to `--port-wait` seconds (default 10, `0` to skip)
for the server to listen; a server that ignores `$PORT` is reached on the port it opened.
`--port` is the port the app is told to use (default 3000; when it is taken, a free one from
5501), and `--stop` stops the workspace's server. The server runs in the [sandbox](#sandbox).
What serves `localhost:4000` starts with the first dev server and exits a few seconds after
the last one stops.

### `forkspace switch <name>`

Point `localhost:4000` at another workspace's dev server. New connections go to the new
server; open ones finish where they are.

## Clean up

### `forkspace drop <names>`

Throw the named workspaces away, stopping whatever runs in them first. Processes that only
have files open there are never killed. A workspace that other workspaces were made from is
dropped only together with them: name them too. `drop` warns when a workspace made commits
that were never merged.

`drop` also clears what nothing uses any more: forkspace's copies of projects no workspace
needs, undo and snapshots past their limits, and what interrupted commands left behind.
Workspaces themselves are never removed by themselves; `ls` shows how old each one is.

## Forkspace itself

### `forkspace setup`

Set up the coding agents on this machine to use forkspace. Today that is Claude Code
(`~/.claude`): `setup` installs the forkspace skill (`~/.claude/skills/forkspace/SKILL.md`)
and adds two hooks to `~/.claude/settings.json`, keeping everything else in it, so the
worktrees Claude Code makes (`claude --worktree`, subagents with `isolation: worktree`,
background sessions) are forkspace workspaces, with `node_modules` and `.env` already
inside. It prints what it added; run again, it changes nothing and says so.

The hooks it adds:

```json
{
  "hooks": {
    "WorktreeCreate": [{"hooks": [{"type": "command", "command": "forkspace hook worktree-create"}]}],
    "WorktreeRemove": [{"hooks": [{"type": "command", "command": "forkspace hook worktree-remove"}]}]
  }
}
```

- `worktree-create` makes a workspace of the project Claude Code runs in (its Git top
  level), named after the worktree, on a new branch `worktree-<name>` when that branch is
  free, and prints its path. A name that already has a workspace of the same project opens
  that workspace again.
- `worktree-remove` drops the workspace. A workspace with changes a merge would bring over,
  or with new commits, is kept and the hook fails, so Claude Code keeps it too.

With the hooks, Claude Code skips `.worktreeinclude` (not needed: ignored files are already
in the workspace) and does not remove the worktrees of subagents; `forkspace ls` and
`forkspace drop` manage them.

### `forkspace upgrade [version]`

`forkspace update` is the same command. Replace the running binary with the latest release,
or with `version` (newer or older). The download is checked against the release's SHA-256
checksum and moved into place in one step. A Cargo install is upgraded with `cargo install`,
and so is a platform the release has no binary for, when Cargo is on `PATH`; a Homebrew
install is left to `brew upgrade`. Needs `curl` and `tar`. Existing workspaces keep working:
state written by an older version is brought up to date when it is next read.

### Shell completion

`forkspace completions <shell>` prints a completion script for `bash`, `zsh`, `fish`,
`elvish` or `powershell`. The zsh script completes workspace names: save it as `_forkspace`
in a directory on `$fpath`. For bash, add `source <(forkspace completions bash)` to
`~/.bashrc`.

## Sandbox

Commands started by `enter` and `run` cannot write to the original project or to other
workspaces: Landlock on Linux, `sandbox-exec` on macOS. Setuid programs such as `sudo` gain no
privileges inside it. Temporary directories (`/tmp`, `$TMPDIR`, `$XDG_RUNTIME_DIR`) and the
dot-entries of your home directory, such as `~/.npm` and `~/.cargo`, stay writable. Because
forkspace's own folder is outside the workspace, run forkspace commands from a shell outside
`enter`.

Each workspace's commands get a private temporary directory,
`$TMPDIR/forkspace-<uid>/<home>/<name>` (owner-only, one `<home>` per `FORKSPACE_HOME`), as
`TMPDIR`, `TMP` and `TEMP`, so parallel workspaces do not meet in fixed `/tmp` paths; `drop`
removes what they leave there. Commands started by `enter` also share one jobserver across all
workspaces, passed as `CARGO_MAKEFLAGS`: parallel `cargo` builds in different workspaces take
turns on one slot per CPU instead of each starting a compiler per CPU. `MAKEFLAGS` is left
alone.

On Linux with a systemd user session, all workspaces run together under one
`forkspace.slice`, with half the CPU weight of your own programs and a shared memory ceiling
of 75%, so agents in workspaces do not make your desktop slow. On macOS, commands that are not
an interactive shell run at background priority.

## Environment

Set by `enter` and `run` for the commands they start:

| Variable | |
| :-- | :-- |
| `FORKSPACE_WORKSPACE` | The workspace's name. |
| `TMPDIR`, `TMP`, `TEMP` | The workspace's private temporary directory. |
| `COMPOSE_PROJECT_NAME` | `<project>-<workspace>`, unless already set, so Docker Compose keeps each workspace's containers apart. |
| `CARGO_MAKEFLAGS` | `enter` only: the shared jobserver. |
| `PORT` | `run` only: the port the app should listen on. |

Read by forkspace:

| Variable | |
| :-- | :-- |
| `FORKSPACE_HOME` | Where workspaces and forkspace's own data live (default `~/.forkspace`, owner-only). |
| `NO_COLOR`, `CLICOLOR_FORCE` | Output is coloured only on a terminal. `NO_COLOR` (any value) turns colour off; `CLICOLOR_FORCE=1` turns it on for pipes too. |

## Exit codes

| Code | Meaning |
| --: | :-- |
| 0 | Success |
| 1 | An error with no more specific code |
| 2 | Command-line usage error |
| 3 | The workspace, or something it needs, was not found |
| 4 | The workspace or branch already exists |
| 5 | Conflict or refusal: nothing was changed |
| 6 | Not possible for this workspace on this machine |
| 7 | Workspaces cannot be made here (for example, no FUSE) |

`enter -- command` exits with the command's own status instead (128 plus the signal number
when a signal ended it).

An error is one line naming both the problem and what to do about it.
