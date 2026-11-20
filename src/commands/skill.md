---
name: forkspace
description: Make disposable copies of a whole project (including node_modules, .env and other ignored files) with forkspace, work in them in isolation, then merge the result back or throw it away. Use when a task needs an isolated workspace, parallel attempts, or a risky experiment.
---

# forkspace

`forkspace new <name>` forks the current project directory into a workspace: a full,
writable view of the project, ignored files included, that the original is never written to
until you merge. Use the workspace's absolute path for every later edit, test and command.

Forkspace is a tool you drive. You stay responsible for choosing the project, the workspace
name, what to run, and for asking before anything changes the user's project or deletes work.

## Before you start

1. Run `forkspace --version` and `forkspace <command> --help` before using a flag you have not
   seen work on this machine. Do not invent commands or flags.
2. Run `forkspace ls` (or `forkspace ls --json`) to see existing workspaces. Do not reuse,
   merge or drop a workspace you did not create unless the user says it is yours to use.
3. Pick a short, unique name for each task or attempt (`fix-login`, `attempt-a`).

## Create and work

```sh
forkspace new fix-login                 # fork the current directory; prints the workspace path
forkspace enter fix-login -- npm test   # run a command inside it (exits with its status)
```

- `new` prints `created workspace '<name>' at <path>`. Edit files at that path. Changing your
  shell's directory does not move other tools; pass the absolute path to each tool call.
- Commands run through `forkspace enter <name> -- <command>` (and `forkspace run`) cannot
  write to the original project or to other workspaces. Prefer it for builds, installs and
  tests. `FORKSPACE_VIEW` holds the workspace path inside such commands.
- `--from <workspace>` stacks a workspace on another one; `--branch <name>` creates a Git branch
  in the workspace for `forkspace land`.
- Each workspace is sealed: later changes to the project reach it only through
  `forkspace pull <name>`.

## Review

```sh
forkspace diff fix-login --stat         # summary; --full for a unified diff, --json for tools
forkspace diff attempt-a attempt-b   # how parallel attempts differ; it never picks one
```

Report what changed with `diff`, not from memory. `compare` lists facts; the user decides.

## Bring changes back: ask first

- `forkspace merge <name> --dry-run` shows the plan without changing anything. Show it to the
  user and ask before running `forkspace merge <name>`, which writes to their project.
- `forkspace undo` puts the project back as it was before the latest merge.
- `forkspace land <name>` brings the workspace's Git commits into the project's repository
  (no working-tree changes). Ask first.
- Exit code 5 means a conflict or refusal and that nothing was changed. Read the message and
  report it; do not retry with `--overwrite-base` or `--theirs` unless the user agrees.

## Clean up: ask first

- Ask before `forkspace drop <name>`. Never run `drop --all`, `drop --glob`, `--force` or
  `--kill`, or `forkspace gc`, without the user's explicit approval: unmerged work in a
  workspace exists nowhere else.
- `drop` refuses while processes run in the workspace and lists them; stop your own
  processes rather than adding `--kill`.
- `forkspace enter --ephemeral -- <command>` makes a throwaway workspace that is removed when
  the command exits: right for one-off experiments.

## Know what is in a workspace

- Everything in the project directory, ignored files (`node_modules`, `.env`, build caches)
  included, except what forks leave out: Git worktrees nested in the project (such as
  `.claude/worktrees/*`) and whatever the project's `.forkspaceexclude` lists.
- Caches outside the project (`~/.npm`, `~/.cargo`) are shared with the user's machine.
- Disk use and speed depend on the backend (`forkspace ls` shows it); a plain copy is
  reported when it happens. Do not promise speed or space savings.

## Claude Code worktrees

`forkspace hook claude-settings` prints a `settings.json` snippet that makes Claude Code's
own worktrees (`claude --worktree`, subagents with `isolation: worktree`) forkspace
workspaces. Ask before changing the user's settings.

## Command summary

```text
forkspace new [name] [--from <dir|workspace>] [--branch <b>] [--copy]
forkspace enter [name] [--create] [--ephemeral] [-- command…]
forkspace run <name> -- <command>          forkspace switch <name>
forkspace diff <name> [--stat|--full|--files|--json]
forkspace diff <a> <b>…                 forkspace ls [--json]
forkspace merge <name> [--dry-run|--commit|--patch|--undo]
forkspace pull <name>                      forkspace land <name>
forkspace checkpoint <name>                forkspace restore <name> <id>
forkspace drop <name>                      forkspace gc
forkspace upgrade (or update)              forkspace skill
```
