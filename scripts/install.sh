#!/bin/sh
# Install the forkspace binary from its GitHub releases:
#
#   curl -fsSL https://github.com/berezovyy/forkspace/releases/latest/download/install.sh | sh
#
# FORKSPACE_VERSION=0.2.0 installs that version instead of the latest one.
# FORKSPACE_INSTALL_DIR chooses where the binary goes (default: ~/.local/bin).
# Upgrade later with `forkspace upgrade`.
set -eu

releases=${FORKSPACE_RELEASES_URL:-https://github.com/berezovyy/forkspace/releases}

say() { printf 'forkspace: %s\n' "$*"; }
die() {
    printf 'forkspace: %s\n' "$*" >&2
    exit 1
}

# Everything runs from main, so a download cut short by the pipe runs nothing.
main() {
    dir=${FORKSPACE_INSTALL_DIR:-$HOME/.local/bin}
    for tool in curl tar; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
    done

    case $(uname -s) in
        Linux) os=unknown-linux-musl ;;
        Darwin) os=apple-darwin ;;
        *) die "no prebuilt binary for $(uname -s); install with: cargo install forkspace" ;;
    esac
    case $(uname -m) in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "no prebuilt binary for $(uname -m); install with: cargo install forkspace" ;;
    esac
    target=$arch-$os

    version=${FORKSPACE_VERSION:-}
    if test -z "$version"; then
        # GitHub redirects releases/latest to releases/tag/<tag>.
        url=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$releases/latest") ||
            die "could not reach $releases"
        case $url in
            */tag/*) version=${url##*/tag/} ;;
            *) die "no release is published at $releases" ;;
        esac
    fi
    version=${version#v}

    asset=forkspace-$version-$target.tar.gz
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT INT TERM
    say "downloading forkspace $version for $target"
    curl -fsSL -o "$tmp/$asset" "$releases/download/v$version/$asset" ||
        die "release $version has no binary for $target (a new release's macOS and arm64 binaries take a few minutes); or install with: cargo install forkspace"
    curl -fsSL -o "$tmp/$asset.sha256" "$releases/download/v$version/$asset.sha256" ||
        die "release $version has no checksum for $asset"
    expected=$(cut -d ' ' -f 1 < "$tmp/$asset.sha256")
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$tmp/$asset" | cut -d ' ' -f 1)
    else
        actual=$(shasum -a 256 "$tmp/$asset" | cut -d ' ' -f 1)
    fi
    test "$expected" = "$actual" || die "checksum mismatch for $asset"
    tar -xzf "$tmp/$asset" -C "$tmp"

    mkdir -p "$dir"
    cp "$tmp/forkspace" "$dir/.forkspace.$$"
    chmod 755 "$dir/.forkspace.$$"
    mv -f "$dir/.forkspace.$$" "$dir/forkspace"
    say "installed $("$dir/forkspace" --version) to $dir/forkspace"

    case ":$PATH:" in
        *":$dir:"*) ;;
        *) say "add it to your PATH: export PATH=\"$dir:\$PATH\"" ;;
    esac
    if test "$os" = unknown-linux-musl && ! command -v fuse-overlayfs >/dev/null 2>&1; then
        say "for the fastest forks, install fuse-overlayfs (for example: sudo apt install fuse-overlayfs)"
    fi
    say "upgrade later with: forkspace upgrade"
}

main "$@"
