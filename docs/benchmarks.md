# Backend benchmarks

Benchmarks are reported per backend; forkspace does not turn one backend's result into a universal
claim. The commands below used an optimized `forkspace 0.1.0` build, five independent forks per
size, one-byte regular files distributed across directories, a fresh `FORKSPACE_HOME` per run,
and a clean `diff --stat` immediately after each fork. Times include CLI startup, materialize,
manifest fsync, state commit, and process exit. Merge changes one existing one-byte file and
includes the manifest scan plus journal fsyncs; allocation is the sum of `st_blocks` for
regular files in the resulting workspace.

## Measured Linux portable-copy baseline

Reference machine: Intel Core i9-9900K 3.60 GHz, Linux 7.0.0-29-generic, local workspace
filesystem, measured 2026-08-11. p95 is the slowest of five samples (a conservative small-N
estimate).

| Entries | Fork p50 | Fork p95 | Clean diff p50 | Clean diff p95 | One-file merge p50 | One-file merge p95 | Workspace allocated after fork |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 4.59 ms | 4.68 ms | 1.55 ms | 1.57 ms | 2.13 ms | 2.17 ms | 400 KiB |
| 1,000 | 32.70 ms | 32.89 ms | 6.42 ms | 6.72 ms | 11.50 ms | 11.65 ms | 3.91 MiB |
| 10,000 | 317.00 ms | 318.78 ms | 60.76 ms | 61.02 ms | 115.91 ms | 117.26 ms | 39.06 MiB |

The copy backend is intentionally O(bytes) and consumes approximately the source tree's full
allocated size. These numbers are a portability floor, not a zero-copy claim.

## Results still requiring release hardware

No native macOS runner or usable `/dev/fuse` was present for this implementation session.
Accordingly, APFS recursive-clone latency/disk sharing and Linux overlay p95 are **not
published as measured facts here**. The release workflows run the same harness on those
hosts; `docs/phases/macos-clone/PROGRESS.md` keeps the release phase provisional until those
results are recorded. Linux reflink is O(entries), with disk sharing verified by the ioctl
probe/CI volume job rather than inferred from a filesystem name.

## Reproduction

Build with `cargo build --release`, generate each tree, then time:

```bash
scratch=$(mktemp -d)
tree="$scratch/tree"
mkdir -p "$tree" "$scratch/home"
printf 'benchmark\n' > "$tree/file"
FORKSPACE_HOME="$scratch/home" forkspace fork "$tree" --name bench --copy
FORKSPACE_HOME="$scratch/home" forkspace diff bench --stat
rm -rf "$scratch"
```

Run at least five fresh-home repetitions, report p50 and p95, and record CPU, OS, filesystem,
backend kind, entry sizes, and whether caches were warm. Never compare an overlay result to a
copy result without naming both backends.
