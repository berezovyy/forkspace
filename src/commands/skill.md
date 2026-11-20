---
name: forkspace
description: Make throwaway copies of a whole project (including node_modules, .env and other ignored files) with forkspace, work in them in isolation, then merge the result into the project or throw it away. Use when a task needs an isolated workspace, parallel attempts, or a risky experiment.
---

# forkspace

`forkspace new <name>` copies the project you are in into a workspace: a full, writable copy,
ignored files included, that leaves the user's project untouched until a merge. Use the
workspace's absolute path for every later edit, test and command.

Forkspace is a tool you drive. You stay responsible for choosing the workspace name, what to
run, and for asking before anything changes the user's project or throws work away.

## Before you start

1. Run `forkspace --version` and `forkspace <command> --help` before relying on something you
   have not seen work on this machine. Do not invent commands or options.
2. Run `forkspace ls` (or `forkspace ls --json`) to see existing workspaces. Do not use,
   merge or drop a workspace you did not make unless the user says it is yours to use.
3. Pick a short, unique name for each task or attempt (`fix-login`, `attempt-a`), and always
   pass it: without a name, commands guess from the current directory.

## Make one and work in it

```sh
forkspace new fix-login                 # copy the project; prints the workspace path
forkspace enter fix-login -- npm test   # run a command inside it (exits with its status)
```

- `new` prints `created workspace '<name>' at <path>`. Edit files at that path. Changing your
  shell's directory does not move other tools; pass the absolute path to each tool call.
- Commands run through `forkspace enter <name> -- <command>` (and `forkspace run`) cannot
  write to the user's project or to other workspaces, and start in the workspace folder.
  Prefer it for builds, installs and tests. Run forkspace itself outside `enter`.
- In a Git project, the workspace is on its own branch, named after it. Commit there as usual.
- A workspace does not see later changes to the project until `forkspace pull <name>`.
- `forkspace save <name> "<note>"` takes a snapshot; `forkspace restore <name>` lists them and
  `forkspace restore <name> <id>` goes back to one.

## Review

```sh
forkspace diff fix-login                # changed files, notes, then a Git patch; --json for tools
forkspace diff attempt-a attempt-b      # how parallel attempts differ; it never picks one
```

Report what changed from `diff`, not from memory. Comparing lists facts; the user decides.

## Bring changes back: ask first

- Show the user `forkspace diff <name>` and ask before `forkspace merge <name>`, which writes
  the changed files into their project (uncommitted) and keeps the workspace's commits as the
  project branch `<name>`.
- `forkspace undo` puts the project, and that branch, back as they were before the latest
  merge.
- Exit code 5 means a conflict or refusal and that nothing was changed. Read the message and
  report it. When it says to pull, `forkspace pull <name>` brings the project's changes into
  the workspace; fix any conflict markers there, then ask before merging again.

## Clean up: ask first

- Ask before `forkspace drop <name>`: it stops what runs in the workspace and throws it away,
  and unmerged work in a workspace exists nowhere else. Name only workspaces you made.

## Know what is in a workspace

- Everything in the project folder, ignored files (`node_modules`, `.env`, build caches)
  included, except Git worktrees nested in the project (such as `.claude/worktrees/*`) and
  whatever the project's `.forkspaceexclude` lists.
- Caches outside the project (`~/.npm`, `~/.cargo`) are shared with the user's machine.
- How a workspace is made depends on the machine; a plain copy is reported when it happens.
  Do not promise speed or space savings.

## Command summary

```text
forkspace new [name]                       forkspace enter [name] [-- command…]
forkspace ls [--json]                      forkspace diff [name…] [--json]
forkspace merge [name]                     forkspace undo [name]
forkspace pull [name] [--json]             forkspace drop <name…>
forkspace save [name] [note]               forkspace restore [name] [id]
forkspace run [name] -- <command>          forkspace switch <name>
forkspace setup                            forkspace upgrade [version]
```
