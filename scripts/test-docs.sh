#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bin="$repo/target/debug/forkspace"
test -x "$bin" || cargo build --manifest-path "$repo/Cargo.toml"

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT INT TERM
manifest="$scratch/blocks.tsv"

git -C "$repo" ls-files README.md 'docs/*.md' 'docs/**/*.md' > "$scratch/docs.list"
python3 - "$repo" "$scratch/docs.list" "$scratch" "$manifest" <<'PY'
import pathlib
import sys

repo = pathlib.Path(sys.argv[1])
listed = pathlib.Path(sys.argv[2]).read_text().splitlines()
out = pathlib.Path(sys.argv[3])
manifest = pathlib.Path(sys.argv[4])
rows = []
index = 0
for relative in listed:
    path = repo / relative
    lines = path.read_text().splitlines(keepends=True)
    pending_skip = False
    in_block = False
    block = []
    start = 0
    skipped = False
    for number, line in enumerate(lines, 1):
        stripped = line.strip()
        if not in_block and stripped.startswith("<!-- notest:") and stripped.endswith("-->"):
            pending_skip = True
            continue
        if not in_block and stripped == "```bash":
            in_block = True
            block = []
            start = number + 1
            skipped = pending_skip
            pending_skip = False
            continue
        if in_block and stripped == "```":
            target = out / f"block-{index:03}.sh"
            target.write_text("".join(block))
            rows.append((str(target), relative, str(start), "1" if skipped else "0"))
            index += 1
            in_block = False
            continue
        if in_block:
            block.append(line)
    if in_block:
        raise SystemExit(f"unterminated bash fence in {relative}:{start - 1}")

manifest.write_text("".join("\t".join(row) + "\n" for row in rows))
PY

notest_count=$(awk -F '	' '$4 == 1 { count++ } END { print count + 0 }' "$manifest")
# Any new exclusion is a review failure. Reducing this constant requires deleting a marker.
test "$notest_count" -eq 2

python3 - "$repo" <<'PY'
import pathlib
import re
import sys

repo = pathlib.Path(sys.argv[1])
matrix = (repo / "docs/platform-parity.md").read_text()
evidence = re.findall(r"`(tests/[^`]+\.rs)::([a-zA-Z0-9_]+)`", matrix)
if not evidence:
    raise SystemExit("platform matrix has no machine-checkable test evidence")
for relative, test in evidence:
    source = (repo / relative).read_text()
    if re.search(rf"\bfn\s+{re.escape(test)}\s*\(", source) is None:
        raise SystemExit(f"missing matrix evidence {relative}::{test}")

readme = (repo / "README.md").read_text()
competitive_section = readme.split("## Why forkspace?", 1)[1].split("## Quick start", 1)[0]
competitive = [
    line
    for line in competitive_section.splitlines()
    if line.startswith("|") and ":---" not in line and "`cp -r`" not in line
]
if len(competitive) != 6 or any("](" not in line for line in competitive):
    raise SystemExit("every competitive-table row must link its evidence")
PY

while IFS='	' read -r block source line skipped; do
    if test "$skipped" = 1; then
        printf 'docs: skip %s:%s (%s)\n' "$source" "$line" "$(sed -n "$((line - 2))p" "$repo/$source")"
        continue
    fi

    case_root="$scratch/case-$(basename "$block" .sh)"
    project="$case_root/project"
    home="$case_root/home"
    mkdir -p "$project/src" "$home"
    printf 'old\n' > "$project/file.txt"
    printf 'pub fn baseline() {}\n' > "$project/src/lib.rs"
    cat > "$project/package.json" <<'JSON'
{"name":"forkspace-docs","version":"1.0.0","private":true,"scripts":{"test":"node -e \"process.exit(0)\""}}
JSON
    cat > "$project/Makefile" <<'MAKE'
test:
	@true
MAKE
    git -C "$project" init --quiet --initial-branch=main
    git -C "$project" config user.email docs@example.invalid
    git -C "$project" config user.name 'Forkspace Docs'
    git -C "$project" add -A
    git -C "$project" commit --quiet -m baseline

    if grep -q 'forkspace compare attempt-a' "$block"; then
        for name in attempt-a attempt-b attempt-c; do
            FORKSPACE_HOME="$home" "$bin" new "$name" --from "$project" --copy >/dev/null 2>/dev/null
        done
        printf 'review\n' > "$home/workspaces/attempt-b/src/review.txt"
    fi

    printf 'docs: run %s:%s\n' "$source" "$line"
    (
        cd "$project"
        PATH="$repo/target/debug:$PATH" \
        FORKSPACE_HOME="$home" \
        FORKSPACE_REPO="$repo" \
        SHELL=/bin/true \
        NO_COLOR=1 \
        sh -eu "$block"
    )
    # Examples may leave workspaces (and overlay mounts) behind; unmount them before cleanup.
    FORKSPACE_HOME="$home" "$bin" drop --all >/dev/null 2>&1 || true
done < "$manifest"

# The documented install command is registry-facing. CI proves the same Cargo package installs
# from a pristine checkout without relying on network publication state.
cargo install --path "$repo" --root "$scratch/install" --locked --offline >/dev/null
"$scratch/install/bin/forkspace" --help >/dev/null
