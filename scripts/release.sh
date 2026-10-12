#!/bin/sh
# Cut a forkspace release from this machine:
#
#   scripts/release.sh 0.2.0
#
# 1. Checks: on master, clean tree, not behind origin, tag unused, logged in to GitHub and
#    crates.io, release notes under `## Unreleased` (or `## 0.2.0`) in CHANGELOG.md.
# 2. Sets the version in Cargo.toml and dates the CHANGELOG section.
# 3. Runs fmt, clippy and the tests (SKIP_TESTS=1 skips them).
# 4. Builds the static Linux x86_64 binary into dist/v0.2.0/ with its checksum and install.sh.
# 5. Commits "release 0.2.0", tags v0.2.0, and dry-runs `cargo publish`.
# 6. Asks, then pushes master and the tag, creates the GitHub release, and publishes the crate.
#
# Everything before the question is local: answering no leaves a commit and a tag to drop. The
# release workflow (.github/workflows/release.yml) then adds the macOS and Linux arm64 binaries
# to the same GitHub release.
set -eu

die() {
    printf 'release: %s\n' "$*" >&2
    exit 1
}
step() { printf '\n==> %s\n' "$*"; }

version=${1:-}
version=${version#v}
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' ||
    die "usage: scripts/release.sh VERSION (for example 0.2.0)"
tag="v$version"
target=x86_64-unknown-linux-musl
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"

step "Preflight"
test "$(uname -s)-$(uname -m)" = Linux-x86_64 || die "the local build is the Linux x86_64 binary; run this on Linux x86_64"
test "$(git branch --show-current)" = master || die "releases are cut from master"
test -z "$(git status --porcelain)" || die "commit or stash your changes first"
git fetch --quiet origin
git merge-base --is-ancestor origin/master HEAD || die "master is behind or has diverged from origin/master"
! git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null || die "tag $tag already exists"
! git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1 || die "tag $tag already exists on origin"
grep -Eq "^## (Unreleased|$version)( |\$)" CHANGELOG.md ||
    die "write the release notes under '## Unreleased' in CHANGELOG.md"
gh auth status >/dev/null 2>&1 || die "log in to GitHub first: gh auth login"
test -n "${CARGO_REGISTRY_TOKEN:-}" || test -f "${CARGO_HOME:-$HOME/.cargo}/credentials.toml" ||
    die "log in to crates.io first: cargo login (token from https://crates.io/settings/tokens)"
rustup target add "$target" >/dev/null 2>&1 || die "rustup target add $target failed"
echo "ok"

step "Set version $version"
awk -v v="$version" '!done && /^version = / { print "version = \"" v "\""; done = 1; next } { print }' \
    Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
awk -v h="## $version - $(date +%Y-%m-%d)" '!done && /^## Unreleased/ { print h; done = 1; next } { print }' \
    CHANGELOG.md > CHANGELOG.md.tmp && mv CHANGELOG.md.tmp CHANGELOG.md
cargo check --quiet # records the new version in Cargo.lock
notes=$(awk -v v="$version" '
    $0 ~ "^## " v "( |$)" { on = 1; next }
    on && /^## / { exit }
    on { print }
' CHANGELOG.md)
test -n "$(printf '%s' "$notes" | tr -d '[:space:]')" || die "the CHANGELOG section for $version is empty"
git --no-pager diff --stat

if test "${SKIP_TESTS:-}" = 1; then
    step "Tests skipped (SKIP_TESTS=1)"
else
    step "Test"
    cargo fmt --check
    cargo clippy --all-targets --locked --quiet -- -D warnings
    cargo test --locked --quiet
fi

step "Build $target"
cargo build --release --locked --target "$target"
out="${CARGO_TARGET_DIR:-target}/$target/release"
binary="$out/forkspace"
test "$("$binary" --version)" = "forkspace $version" || die "the built binary reports $("$binary" --version)"
dist="$repo/dist/$tag"
rm -rf "$dist"
mkdir -p "$dist"
asset="forkspace-$version-$target.tar.gz"
tar -czf "$dist/$asset" -C "$out" forkspace -C "$repo" LICENSE
(cd "$dist" && sha256sum "$asset" > "$asset.sha256")
cp scripts/install.sh "$dist/install.sh"
printf '%s\n' "$notes" > "$dist/notes.md"
ls -l "$dist"

step "Commit and tag $tag"
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit --quiet -m "release $version"
git tag -a "$tag" -m "forkspace $version"
git --no-pager log --oneline -1

step "Check the crate"
cargo publish --dry-run --locked --quiet

prerelease=
case $version in *-*) prerelease=--prerelease ;; esac
cat <<EOF

Ready to publish forkspace $version:
  - push master and $tag to origin
  - GitHub release $tag with $asset and install.sh
    (the release workflow then adds the macOS and Linux arm64 binaries)
  - crates.io: forkspace $version (permanent: a version can be yanked, never deleted)

EOF
printf 'Publish? [y/N] '
read -r answer
case $answer in
    y | Y | yes) ;;
    *)
        printf '\nNothing was published. To undo the release commit and tag:\n'
        printf '  git tag -d %s && git reset --hard HEAD~1\n' "$tag"
        exit 0
        ;;
esac

step "Push"
git push origin master "refs/tags/$tag"

step "GitHub release"
gh release create "$tag" "$dist/$asset" "$dist/$asset.sha256" "$dist/install.sh" \
    --verify-tag --title "forkspace $version" --notes-file "$dist/notes.md" $prerelease

step "crates.io"
cargo publish --locked

slug=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
cat <<EOF

Released forkspace $version.
  https://github.com/$slug/releases/tag/$tag
  https://crates.io/crates/forkspace/$version
macOS and Linux arm64 binaries are building: gh run list --workflow release.yml
EOF
