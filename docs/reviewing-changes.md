# Reviewing parallel changes

Make one workspace per attempt, let each attempt run, then review them side by side:

```bash
forkspace diff attempt-a attempt-b attempt-c   # how the attempts differ
forkspace diff attempt-b                       # one attempt in full
forkspace merge attempt-b                      # keep it
forkspace drop attempt-a attempt-c             # throw the others away
```

`diff` with several names shortlists on mechanical facts. Contested paths were touched by
every attempt; unique paths show where one attempt went beyond the others; partial paths were
shared by only some attempts. Dependency changes name packages only when a supported lockfile
parses. The new/edited ratio is context, not a judgment: mostly-new files can mean a proper
new module or isolated pasted code. The reviewer decides. Test-file counts go by file names
and paths, miss Rust inline `#[cfg(test)]` modules and table-driven tests in ordinary files,
and can mistake an OpenAPI `spec/` directory for tests.

## What forkspace deliberately does not do

Forkspace does not auto-review, rank, grade, score, pick, or block an attempt. Its notes are
never lint thresholds. The gate is the person who will own the change; forkspace only makes the
facts cheap to inspect and the rejected attempts cheap to throw away.
