#!/bin/sh
# Install ral (default) or exarch on macOS or Linux, from the newest versioned
# release or a named one, e.g. the rolling `latest`.
# Usage: curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh
#        curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh -s exarch
#        curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh -s ral latest
#
# Everything below is one function, called on the last line.  A script read
# from a pipe is executed as it arrives, so a connection that dies mid-transfer
# would otherwise run whatever prefix had landed — half an installer, with no
# error.  Defined this way, a truncated download cannot call anything.
set -eu

REPO="lambdabetaeta/ral"

die() {
    echo "install.sh: $*" >&2
    exit 1
}

# github.com/$REPO/releases/latest 302s to the newest *versioned* release, not
# the rolling release tagged `latest`.  Resolved once, so the archive and its
# checksum come from the same release.
latest_tag() {
    final="$(curl -fsSL --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' \
        "https://github.com/${REPO}/releases/latest")" || die "could not reach github.com/${REPO}"
    case "$final" in
        */tag/?*) echo "${final##*/tag/}" ;;
        *) die "could not resolve the latest release tag for ${REPO}" ;;
    esac
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
    product="${1:-ral}"
    case "$product" in
        ral)    licenses="LICENSE-APACHE LICENSE-MIT NOTICE" ;;
        exarch) licenses="LICENSE-APACHE LICENSE-MIT NOTICE LICENSE-EDTUI" ;;
        *) die "unknown product: $product (expected ral or exarch)" ;;
    esac

    command -v curl > /dev/null 2>&1 || die "curl is required"
    command -v tar > /dev/null 2>&1 || die "tar is required"

    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os/$arch" in
        Darwin/arm64)   target="macos-arm64"   ;;
        Darwin/x86_64)
            echo "No native x86_64 macOS build; installing arm64 (needs Rosetta 2)."
            target="macos-arm64"
            ;;
        Linux/x86_64)   target="linux-x86_64"  ;;
        Linux/aarch64)  target="linux-arm64"   ;;
        Linux/arm64)    target="linux-arm64"   ;;
        *)
            hint=""
            if [ "$product" = exarch ]; then hint="; on Windows, use exarch-install.ps1"; fi
            die "unsupported platform: $os $arch$hint"
            ;;
    esac

    tag="${2:-$(latest_tag)}"
    artifact="${product}-${target}"
    archive="${artifact}.tar.gz"
    url="https://github.com/${REPO}/releases/download/${tag}/${archive}"

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT INT TERM

    echo "Downloading ${archive} from ${REPO} (${tag})"
    curl -fL --proto '=https' --tlsv1.2 --progress-bar "$url" -o "$tmp/$archive"
    curl -fL --proto '=https' --tlsv1.2 --silent "$url.sha256" -o "$tmp/$archive.sha256"

    verify "$tmp/$archive" "$(cut -d ' ' -f 1 < "$tmp/$archive.sha256")"
    echo "Checksum OK."
    # shellcheck disable=SC2086 # $licenses is a word list
    tar -xzf "$tmp/$archive" -C "$tmp" "$artifact" $licenses

    data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/${product}"
    mkdir -p "$data_dir"
    for file in $licenses; do
        install -m 644 "$tmp/$file" "$data_dir/$file"
    done

    if [ -w /usr/local/bin ]; then
        dir="/usr/local/bin"
    else
        dir="$HOME/.local/bin"
        mkdir -p "$dir"
    fi

    # In through the neighbouring name, then a rename within one directory —
    # which is atomic, and which, unlike writing through the path, does not
    # fail with ETXTBSY when the binary being replaced is currently running:
    # that process keeps the old inode and the next one gets the new.  The
    # staging move may well cross a filesystem and copy; the move that matters
    # cannot.
    chmod 755 "$tmp/$artifact"
    staged="$dir/.${product}.$$"
    mv "$tmp/$artifact" "$staged" || die "could not write to $dir"
    if [ "$os" = Darwin ]; then codesign -s - "$staged" 2> /dev/null || true; fi
    mv "$staged" "$dir/$product" || { rm -f "$staged"; die "could not install into $dir"; }
    echo "Installed $dir/$product"
    if [ "$product" = ral ]; then
        # ral-sh is ral by another name: the POSIX bridge for chsh (SPEC §16.7).
        ln -sfn ral "$dir/ral-sh"
        echo "Linked $dir/ral-sh"
    fi

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
    case "$product" in
        ral)
            echo "ral is ready.  Run: ral"
            echo "To make it your login shell, register the bridge and switch:"
            echo
            echo "  sudo sh -c 'echo $dir/ral-sh >> /etc/shells'"
            echo "  chsh -s $dir/ral-sh"
            ;;
        exarch)
            echo "exarch is ready.  The ral shell it runs on is built in; no separate install."
            echo "Set a provider key in your environment, then run exarch from a project:"
            echo "  ANTHROPIC_API_KEY=… exarch"
            ;;
    esac
}

main "$@"
