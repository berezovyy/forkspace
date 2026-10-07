#!/bin/sh
# Build the pinned benchmark corpus once and print the path of its cached tarball.
#
# The corpus is a fresh create-next-app@15.5.4 project (TypeScript, ESLint, Tailwind, App
# Router) installed with `npm ci` from the lockfile committed in scripts/bench-corpus/, then
# committed to Git with a .env beside it, as in the README film. Only the first run needs the
# network; later runs print the cached tarball.
#
#   scripts/bench-corpus.sh                 # build if missing, print the tarball path
#   scripts/bench-corpus.sh --lock          # regenerate the committed lockfile (maintainers)
set -eu

version=15.5.4
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
pinned="$repo/scripts/bench-corpus"
cache=${FORKSPACE_BENCH_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/forkspace-bench}
tarball="$cache/next-$version.tar.gz"

lock=false
if [ "${1:-}" = "--lock" ]; then
    lock=true
fi
if [ -f "$tarball" ] && ! $lock; then
    echo "$tarball"
    exit 0
fi

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT INT TERM
mkdir -p "$cache"

# Saved create-next-app preferences must not change the result.
XDG_CONFIG_HOME="$scratch/config" npx --yes "create-next-app@$version" "$scratch/next" \
    --ts --eslint --tailwind --app --import-alias '@/*' --use-npm \
    --skip-install --disable-git --yes >&2

cd "$scratch/next"
if $lock || [ ! -f "$pinned/package-lock.json" ]; then
    npm install --no-audit --no-fund >&2
    mkdir -p "$pinned"
    cp package.json package-lock.json "$pinned/"
    echo "bench-corpus: wrote $pinned/package-lock.json; commit it to pin the corpus" >&2
else
    cp "$pinned/package.json" "$pinned/package-lock.json" .
    npm ci --no-audit --no-fund >&2
fi

printf 'DATABASE_URL=postgres://localhost/bench\n' > .env
git init -q
git add -A
git -c user.name=bench -c user.email=bench@localhost commit -qm corpus
cd "$scratch"
tar -czf "$tarball.tmp" next
mv "$tarball.tmp" "$tarball"
echo "$tarball"
