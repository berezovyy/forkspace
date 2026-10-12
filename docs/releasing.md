# Releasing

A release is one command on the maintainer's Linux x86_64 machine, plus a GitHub workflow for
the binaries that machine cannot build.

| Channel | What users run | Where it comes from |
| :-- | :-- | :-- |
| Prebuilt binary | `curl -fsSL https://github.com/berezovyy/forkspace/releases/latest/download/install.sh \| sh` | GitHub release assets |
| crates.io | `cargo install forkspace` | `cargo publish` |
| Upgrade | `forkspace upgrade`, `forkspace upgrade 0.2.0`, `--check` | GitHub release assets, or Cargo for a Cargo install |

## Once per machine

```sh
gh auth login                      # GitHub CLI, to create releases
cargo login                        # token from https://crates.io/settings/tokens
rustup target add x86_64-unknown-linux-musl
```

## Every release

1. Write the notes under `## Unreleased` in [`CHANGELOG.md`](../CHANGELOG.md) and commit
   everything; the script refuses a dirty tree.
2. Pick the version. Before 1.0: `0.x.0` when commands or behaviour change, `0.x.y` for fixes.
   A version like `0.3.0-rc.1` is published as a pre-release, which `install.sh` and
   `forkspace upgrade` skip unless asked for it by name.
3. Run:

   ```sh
   scripts/release.sh 0.2.0
   ```

   It checks the tree, sets the version, runs fmt, clippy and the tests, builds the static
   Linux x86_64 binary into `dist/v0.2.0/`, commits `release 0.2.0`, tags `v0.2.0`, and
   dry-runs `cargo publish`. Then it asks. Up to that question nothing has left the machine,
   and `git tag -d v0.2.0 && git reset --hard HEAD~1` undoes it. On yes it pushes master and
   the tag, creates the GitHub release with the Linux binary, its checksum and `install.sh`,
   and publishes the crate.
4. The [`release` workflow](../.github/workflows/release.yml) starts when the release is
   published and uploads the macOS arm64, macOS x86_64 and Linux arm64 binaries to it, a few
   minutes later. Watch it with `gh run list --workflow release.yml`; rerun it for a tag from
   the Actions tab if a build fails.

A crates.io version is permanent: it can be yanked (`cargo yank --version 0.2.0`) so new
installs skip it, never deleted or replaced. Fix a bad release with a new patch version.

## Release assets

Each platform gets `forkspace-<version>-<target>.tar.gz` (the `forkspace` binary and
`LICENSE`) and a `.sha256` beside it. The targets are `x86_64-unknown-linux-musl`,
`aarch64-unknown-linux-musl`, `x86_64-apple-darwin` and `aarch64-apple-darwin`. Linux binaries
are static, so one build runs on every distribution. `install.sh` and `forkspace upgrade`
build the asset name from the platform, so these names are a contract: renaming them breaks
upgrades from every older version.

## Upgrades and state

Workspaces outlive the binary that made them. State on disk carries a `version`, and
[`src/state/migrate.rs`](../src/state/migrate.rs) brings older state forward when it is read.
A release that changes the state format bumps that version and adds a migration step.
