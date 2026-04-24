#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bin="$repo/target/debug/forkspace"
test -x "$bin" || cargo build --manifest-path "$repo/Cargo.toml"

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT INT TERM
mkdir "$scratch/project" "$scratch/home"
printf 'old\n' > "$scratch/project/file.txt"

FORKSPACE_HOME="$scratch/home" "$bin" fork "$scratch/project" --name docs --copy >/dev/null
printf 'new\n' > "$scratch/home/workspaces/docs/file.txt"
FORKSPACE_HOME="$scratch/home" "$bin" diff docs --stat | grep '1 file changed' >/dev/null
FORKSPACE_HOME="$scratch/home" "$bin" merge docs --no-install --drop >/dev/null
test "$(cat "$scratch/project/file.txt")" = new

# Non-runnable fences are an explicit, monotonically decreasing allowance.
count=$(grep -R '<!-- notest:' "$repo/README.md" "$repo/docs" --include='*.md' | wc -l)
test "$count" -le 2
