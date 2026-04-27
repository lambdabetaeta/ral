#!/bin/sh
# Install ral.
# Usage: curl -fsSL https://lambdabetaeta.github.io/ral/scripts/install.sh | sh
set -e

REPO="lambdabetaeta/ral"
TAG="latest"

# ── Platform detection ────────────────────────────────────────────────────────

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Darwin)
        case "$arch" in
            arm64)  artifact="ral-macos-arm64" ;;
            x86_64)
                echo "No native x86_64 macOS build; using arm64 (requires Rosetta 2)."
                artifact="ral-macos-arm64"
                ;;
            *) echo "Unsupported macOS architecture: $arch" >&2; exit 1 ;;
        esac
        ;;
    Linux)
        case "$arch" in
            x86_64)  artifact="ral-linux-x86_64" ;;
            aarch64) artifact="ral-linux-arm64"   ;;
            *) echo "Unsupported Linux architecture: $arch" >&2; exit 1 ;;
        esac
        ;;
    *)
        echo "Unsupported OS: $os" >&2
        exit 1
        ;;
esac

# ── Download ──────────────────────────────────────────────────────────────────

url="https://github.com/${REPO}/releases/download/${TAG}/${artifact}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading ${artifact} from ${REPO} (${TAG})"
curl -fL --progress-bar "$url"        -o "${tmp}/ral"
curl -fL          --silent "$url.sha256" -o "${tmp}/ral.sha256"

expected="$(cat "${tmp}/ral.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "${tmp}/ral" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "${tmp}/ral" | cut -d' ' -f1)"
else
    echo "Warning: sha256sum and shasum not found; skipping checksum verification." >&2
    actual="$expected"
fi
if [ "$actual" != "$expected" ]; then
    echo "Checksum mismatch!" >&2
    echo "  expected: $expected" >&2
    echo "  got:      $actual" >&2
    exit 1
fi
echo "Checksum OK."

# ── Install binary ────────────────────────────────────────────────────────────

if [ -d "/usr/local/bin" ] && [ -w "/usr/local/bin" ]; then
    install_dir="/usr/local/bin"
else
    install_dir="${HOME}/.local/bin"
    mkdir -p "$install_dir"
fi

install -m 755 "${tmp}/ral" "${install_dir}/ral"
echo "Installed ${install_dir}/ral"

if [ "$os" = "Darwin" ]; then
    codesign -s - "${install_dir}/ral" 2>/dev/null || true
fi

case ":${PATH}:" in
    *":${install_dir}:"*) ;;
    *)
        echo ""
        echo "Note: ${install_dir} is not in your PATH."
        echo "Add to your shell profile (~/.zshrc, ~/.bashrc, etc.):"
        echo ""
        echo "  export PATH=\"${install_dir}:\$PATH\""
        echo ""
        ;;
esac

# ── Install rc file ───────────────────────────────────────────────────────────

if [ -n "$XDG_CONFIG_HOME" ]; then
    rc_dir="${XDG_CONFIG_HOME}/ral"
else
    rc_dir="${HOME}/.config/ral"
fi
rc_file="${rc_dir}/rc"

if [ -f "$rc_file" ]; then
    echo "Existing rc found at ${rc_file}; leaving it untouched."
else
    mkdir -p "$rc_dir"
    cat > "$rc_file" <<'RALRC'
# ~/.config/ral/rc — loaded by the interactive shell at startup.
#
# This file must return a map.  All keys are optional:
#   edit_mode — "emacs" (default) or "vi"
#   prompt    — block returning a prompt string; $USER, $CWD, $STATUS available
#   env       — map of environment variables inherited by child processes
#   bindings  — map of bindings available at the prompt
#   aliases   — map of command aliases (strings or blocks)

let red   = "\e[31m"
let blue  = "\e[34m"
let reset = "\e[0m"

return [
    edit_mode: emacs,

    env: [
        EDITOR: vi,
        PAGER:  less,
    ],

    prompt: {
        let lbracket = '['
        let rbracket = ']'
        return "$red$lbracket $USER $rbracket$reset $blue$CWD$reset $ "
    },

    aliases: [
        ll: { |args| ls -lh ...$args },
        la: { |args| ls -lha ...$args },
        g:  { |args| git ...$args },
    ],
]
RALRC
    echo "Wrote default rc to ${rc_file}"
fi

echo ""
echo "ral is ready.  Run: ral"
