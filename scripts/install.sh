#!/bin/sh
# Install ral on macOS or Linux.
# Usage: curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh
#
# Everything below is one function, called on the last line.  A script read
# from a pipe is executed as it arrives, so a connection that dies mid-transfer
# would otherwise run whatever prefix had landed — half an installer, with no
# error.  Defined this way, a truncated download cannot call anything.
set -eu

REPO="lambdabetaeta/ral"
TAG="latest"

die() {
    echo "install.sh: $*" >&2
    exit 1
}

# The checksum published beside a release asset comes from the same origin as
# the asset, so it proves the download arrived intact, not that the release is
# ours.  That is worth having and worth not overstating: it catches truncation,
# a corrupted proxy, and a stale CDN copy.  It is not a signature.
verify() {
    file="$1"
    expected="$2"
    if command -v sha256sum > /dev/null 2>&1; then
        actual="$(sha256sum "$file" | cut -d ' ' -f 1)"
    elif command -v shasum > /dev/null 2>&1; then
        actual="$(shasum -a 256 "$file" | cut -d ' ' -f 1)"
    elif command -v openssl > /dev/null 2>&1; then
        actual="$(openssl dgst -sha256 "$file" | tr ' ' '\n' | tail -n 1)"
    else
        die "no sha256sum, shasum, or openssl: cannot verify the download"
    fi
    [ "$actual" = "$expected" ] || die "checksum mismatch
  expected: $expected
  actual:   $actual"
}

main() {
    command -v curl > /dev/null 2>&1 || die "curl is required"

    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os/$arch" in
        Darwin/arm64)   artifact="ral-macos-arm64"   ;;
        Darwin/x86_64)
            echo "No native x86_64 macOS build; installing arm64 (needs Rosetta 2)."
            artifact="ral-macos-arm64"
            ;;
        Linux/x86_64)   artifact="ral-linux-x86_64"  ;;
        Linux/aarch64)  artifact="ral-linux-arm64"   ;;
        Linux/arm64)    artifact="ral-linux-arm64"   ;;
        *) die "unsupported platform: $os $arch" ;;
    esac

    url="https://github.com/${REPO}/releases/download/${TAG}/${artifact}"

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT INT TERM

    echo "Downloading ${artifact} from ${REPO} (${TAG})"
    curl -fL --proto '=https' --tlsv1.2 --progress-bar "$url" -o "$tmp/ral"
    curl -fL --proto '=https' --tlsv1.2 --silent "$url.sha256" -o "$tmp/ral.sha256"

    verify "$tmp/ral" "$(cut -d ' ' -f 1 < "$tmp/ral.sha256")"
    echo "Checksum OK."

    if [ -w /usr/local/bin ]; then
        dir="/usr/local/bin"
    else
        dir="$HOME/.local/bin"
        mkdir -p "$dir"
    fi

    # In through the neighbouring name, then a rename within one directory —
    # which is atomic, and which, unlike writing through the path, does not
    # fail with ETXTBSY when the ral being replaced is currently running: that
    # process keeps the old inode and the next one gets the new.  The staging
    # move may well cross a filesystem and copy; the move that matters cannot.
    chmod 755 "$tmp/ral"
    staged="$dir/.ral.$$"
    mv "$tmp/ral" "$staged" || die "could not write to $dir"
    mv "$staged" "$dir/ral" || { rm -f "$staged"; die "could not install into $dir"; }
    echo "Installed $dir/ral"

    case ":$PATH:" in
        *":$dir:"*) ;;
        *)
            echo
            echo "Note: $dir is not on your PATH."
            echo "Add this to your shell profile (~/.zshrc, ~/.bashrc, ...):"
            echo
            echo "  export PATH=\"$dir:\$PATH\""
            echo
            ;;
    esac

    echo
    echo "ral is ready.  Run: ral"
}

main "$@"
