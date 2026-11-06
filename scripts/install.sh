#!/bin/sh
# Install ral (default) or exarch on macOS or Linux, from the newest release or
# the one a tag names.
# Usage: curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh
#        curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh -s exarch
#        curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh -s ral v0.10.0
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

sgr() {
    if [ "$depth" -gt 0 ]; then printf '\033[%sm' "$1"; fi
}

# mark SIGIL VERB DETAIL: one line of progress.  * doing, + done, ! heed.
mark() {
    case "$1" in
        '*') ink="$(sgr 36)" ;;
        +) ink="$(sgr 32)" ;;
        !) ink="$(sgr 33)" ;;
        *) die "mark: unknown sigil: $1" ;;
    esac
    printf '  %s[%s]%s %s%-8s%s %s\n' "$ink" "$1" "$(sgr 0)" "$(sgr 1)" "$2" "$(sgr 0)" "$3"
}

# Paints a figure in the ANSI Shadow face with a diagonal gradient from colour
# $1 to $2.  The figure is drawn in ASCII, one byte per glyph: # █, = ═, | ║,
# and the corners r ╔, 7 ╗, L ╚, J ╝.  At 8 bits, q picks the nearest level of
# the xterm cube: 0, 95, 135, 175, 215, 255.
gradient() {
    awk -v from="$1" -v to="$2" -v depth="$depth" '
        BEGIN {
            split(from, lo); split(to, hi)
            glyph["#"] = "█"; glyph["="] = "═"; glyph["|"] = "║"
            glyph["r"] = "╔"; glyph["7"] = "╗"; glyph["L"] = "╚"; glyph["J"] = "╝"
        }
        function q(v) { return v < 48 ? 0 : v < 115 ? 1 : int((v - 35) / 40) }
        function ink(t,   r, g, b) {
            r = lo[1] + (hi[1] - lo[1]) * t
            g = lo[2] + (hi[2] - lo[2]) * t
            b = lo[3] + (hi[3] - lo[3]) * t
            if (depth == 24) return sprintf("\033[38;2;%d;%d;%dm", r, g, b)
            if (depth == 8) return sprintf("\033[38;5;%dm", 16 + 36 * q(r) + 6 * q(g) + q(b))
            return ""
        }
        { row[NR] = $0; if (length($0) > width) width = length($0) }
        END {
            for (y = 1; y <= NR; y++) {
                line = "  "
                for (x = 1; x <= length(row[y]); x++) {
                    c = substr(row[y], x, 1)
                    line = line (c == " " ? c : ink((x + 2 * y) / (width + 2 * NR)) glyph[c])
                }
                print line (depth ? "\033[0m" : "")
            }
        }'
}

banner() {
    echo
    case "$1" in
        ral)
            gradient "40 220 255" "190 100 255" << 'EOF'
######7  #####7 ##7
##r==##7##r==##7##|
######rJ#######|##|
##r==##7##r==##|##|
##|  ##|##|  ##|#######7
L=J  L=JL=J  L=JL======J
EOF
            tagline="a shell based on call-by-push-value"
            ;;
        exarch)
            gradient "190 100 255" "255 120 80" << 'EOF'
#######7##7  ##7 #####7 ######7  ######7##7  ##7
##r====JL##7##rJ##r==##7##r==##7##r====J##|  ##|
#####7   L###rJ #######|######rJ##|     #######|
##r==J   ##r##7 ##r==##|##r==##7##|     ##r==##|
#######7##rJ ##7##|  ##|##|  ##|L######7##|  ##|
L======JL=J  L=JL=J  L=JL=J  L=J L=====JL=J  L=J
EOF
            tagline="a coding agent whose only tool is the shell"
            ;;
    esac
    printf '  %s%s%s\n\n' "$(sgr 2)" "$tagline" "$(sgr 0)"
}

# HTTPS only, redirects included; an HTTP error status is a failure.
fetch() {
    curl -fLS --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 3 "$@"
}

# github.com/$REPO/releases/latest 302s to the newest release's tag.  Resolved
# once, so the archive and its checksum come from the same release.
latest_tag() {
    final="$(fetch -s -o /dev/null -w '%{url_effective}' \
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
    # Spelled out: a range such as a-f is locale-dependent.
    case "$expected" in
        "" | *[!0123456789abcdef]*) die "the published checksum is not a SHA-256 digest: '$expected'" ;;
    esac
    if command -v sha256sum > /dev/null 2>&1; then
        actual="$(sha256sum "$file" | cut -d ' ' -f 1)"
    elif command -v shasum > /dev/null 2>&1; then
        actual="$(shasum -a 256 "$file" | cut -d ' ' -f 1)"
    elif command -v openssl > /dev/null 2>&1; then
        actual="$(openssl dgst -sha256 "$file" | tr ' ' '\n' | tail -n 1)"
    else
        die "no sha256sum, shasum, or openssl: cannot verify the download"
    fi
    [ "$actual" = "$expected" ] || die "checksum mismatch; the download is damaged, so try again
  expected: $expected
  actual:   $actual"
}

# Removes the scratch directory, and a staged binary that never made it in.
cleanup() {
    rm -rf "$tmp"
    if [ -n "${staged:-}" ]; then rm -f "$staged"; fi
}

main() {
    [ "$#" -le 2 ] || die "usage: install.sh [ral|exarch] [TAG]"
    product="${1:-ral}"
    case "$product" in
        ral)    licenses="LICENSE-APACHE LICENSE-MIT NOTICE" ;;
        exarch) licenses="LICENSE-APACHE LICENSE-MIT NOTICE LICENSE-EDTUI" ;;
        *) die "unknown product: $product (expected ral or exarch)" ;;
    esac

    # Bits of colour: none off a terminal, or under NO_COLOR (no-color.org).
    depth=0
    if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != dumb ]; then
        case "${COLORTERM:-}" in
            truecolor | 24bit) depth=24 ;;
            *) depth=8 ;;
        esac
    fi
    banner "$product"

    command -v curl > /dev/null 2>&1 || die "curl is required"
    command -v tar > /dev/null 2>&1 || die "tar is required"
    [ -n "${HOME:-}" ] || die "HOME is not set"

    os="$(uname -s)"
    arch="$(uname -m)"
    source_hint="build from source instead: https://github.com/${REPO}"
    case "$os/$arch" in
        Darwin/arm64)   target="macos-arm64"   ;;
        Darwin/x86_64)
            # Under Rosetta, a shell on Apple silicon reports x86_64.
            [ "$(sysctl -n sysctl.proc_translated 2> /dev/null)" = 1 ] \
                || die "there is no build for Intel Macs; $source_hint"
            target="macos-arm64"
            ;;
        Linux/x86_64)   target="linux-x86_64"  ;;
        Linux/aarch64)  target="linux-arm64"   ;;
        Linux/arm64)    target="linux-arm64"   ;;
        MINGW*/* | MSYS*/* | CYGWIN*/*)
            case "$product" in
                ral) die "on Windows, install ral with ral-windows.msi from https://github.com/${REPO}/releases" ;;
                exarch) die "on Windows, install exarch from PowerShell: irm https://lambdabetaeta.github.io/ral/scripts/exarch-install.ps1 | iex" ;;
            esac
            ;;
        *) die "there is no build for $os $arch; $source_hint" ;;
    esac

    tag="${2:-$(latest_tag)}"
    artifact="${product}-${target}"
    archive="${artifact}.tar.gz"
    url="https://github.com/${REPO}/releases/download/${tag}/${archive}"
    mark '*' release "${REPO} ${tag}"

    tmp="$(mktemp -d)"
    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    mark '*' fetch "$archive"
    if [ -t 2 ]; then progress=--progress-bar; else progress=--silent; fi
    fetch "$progress" -o "$tmp/$archive" "$url" \
        || die "could not download $url; does release $tag exist, and ship $archive?"
    fetch --silent -o "$tmp/$archive.sha256" "$url.sha256" \
        || die "could not download the checksum $url.sha256"

    sum="$(cut -d ' ' -f 1 < "$tmp/$archive.sha256")"
    verify "$tmp/$archive" "$sum"
    mark + verify "sha256 $(printf '%.16s' "$sum")…"
    # shellcheck disable=SC2086 # $licenses is a word list
    tar -xzf "$tmp/$archive" -C "$tmp" "$artifact" $licenses \
        || die "could not unpack $artifact and its licences from $archive"

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
    # mv onto a directory would move into it.
    [ ! -d "$dir/$product" ] || die "$dir/$product is a directory; move it aside and rerun"

    # In through the neighbouring name, then a rename within one directory —
    # which is atomic, and which, unlike writing through the path, does not
    # fail with ETXTBSY when the binary being replaced is currently running:
    # that process keeps the old inode and the next one gets the new.  The
    # staging copy is a fresh file, so it is ours: tar run as root would keep
    # the archive's owner, and that user could then rewrite the binary.
    staged="$dir/.${product}.$$"
    install -m 755 "$tmp/$artifact" "$staged" || die "could not write to $dir"
    if [ "$os" = Darwin ]; then codesign -s - "$staged" 2> /dev/null || true; fi
    "$staged" --help > /dev/null 2>&1 || die "the downloaded $product does not run on this $os $arch machine"
    mv "$staged" "$dir/$product" || die "could not install into $dir"
    mark + install "$dir/$product"
    if [ "$product" = ral ]; then
        # ral-sh is ral by another name: the POSIX bridge for chsh (SPEC §16.7).
        ln -sfn ral "$dir/ral-sh"
        mark + link "$dir/ral-sh -> ral"
    fi

    case ":$PATH:" in
        *":$dir:"*) ;;
        *)
            mark ! path "$dir is not on your PATH; add this to your shell profile:"
            printf "\n    export PATH=\"%s:\$PATH\"\n" "$dir"
            ;;
    esac

    echo
    case "$product" in
        ral)
            printf '  %sral is ready.%s  Run: ral\n' "$(sgr 1)" "$(sgr 0)"
            printf '  To make it your login shell:\n\n'
            if ! grep -qxF "$dir/ral-sh" /etc/shells 2> /dev/null; then
                printf "    sudo sh -c 'echo %s >> /etc/shells'\n" "$dir/ral-sh"
            fi
            printf '    chsh -s %s\n' "$dir/ral-sh"
            ;;
        exarch)
            printf '  %sexarch is ready.%s\n' "$(sgr 1)" "$(sgr 0)"
            printf '  Set a provider key in your environment, then run exarch from a project:\n\n'
            printf '    ANTHROPIC_API_KEY=… exarch\n'
            ;;
    esac
    echo
}

main "$@"
