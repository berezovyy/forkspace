# Reviewing parallel changes

Create one disposable workspace per attempt, then use the following human review loop:

```bash
forkspace compare attempt-a attempt-b attempt-c
forkspace diff attempt-b --stat
forkspace diff attempt-b --full
forkspace merge attempt-b --dry-run
forkspace merge attempt-b
forkspace drop attempt-a && forkspace drop attempt-c
```

`compare` shortlists on mechanical facts. Contested paths were touched by every attempt;
unique paths show where one attempt expanded its scope; partial paths were shared by only
some attempts. Dependency deltas name packages only when a supported lockfile parses. The
new/edited ratio is context, not a judgment: mostly-new files can mean an appropriate new
module or isolated pasted code. The reviewer decides. Test-file counts use a filename/path
heuristic, miss Rust inline `#[cfg(test)]` modules and table-driven tests in ordinary files,
and can mistake an OpenAPI `spec/` directory for tests.

## What forkspace deliberately does not do

Forkspace does not auto-review, rank, grade, score, pick, or block an attempt. Signals are never
lint thresholds. The gate is the person who will own the change; forkspace only makes the facts
cheap enough to inspect and the rejected attempts cheap enough to discard.
