#!/bin/sh
# Install exarch.
# Usage: curl -fsSL https://lambdabetaeta.github.io/ral/scripts/exarch-install.sh | sh
set -e

REPO="lambdabetaeta/ral"
TAG="latest"

# ── Platform detection ────────────────────────────────────────────────────────

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Darwin)
        case "$arch" in
            arm64)  artifact="exarch-macos-arm64" ;;
            x86_64)
                echo "No native x86_64 macOS build; using arm64 (requires Rosetta 2)."
                artifact="exarch-macos-arm64"
                ;;
            *) echo "Unsupported macOS architecture: $arch" >&2; exit 1 ;;
        esac
        ;;
    Linux)
        case "$arch" in
            x86_64)  artifact="exarch-linux-x86_64" ;;
            aarch64) artifact="exarch-linux-arm64"   ;;
            *) echo "Unsupported Linux architecture: $arch" >&2; exit 1 ;;
        esac
        ;;
    *)
        echo "Unsupported OS: $os" >&2
        echo "This installer covers macOS and Linux only; on Windows, use exarch-install.ps1 instead." >&2
        exit 1
        ;;
esac

# ── Download ──────────────────────────────────────────────────────────────────

url="https://github.com/${REPO}/releases/download/${TAG}/${artifact}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading ${artifact} from ${REPO} (${TAG})"
curl -fL --progress-bar "$url"        -o "${tmp}/exarch"
curl -fL          --silent "$url.sha256" -o "${tmp}/exarch.sha256"

expected="$(cat "${tmp}/exarch.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "${tmp}/exarch" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "${tmp}/exarch" | cut -d' ' -f1)"
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

install -m 755 "${tmp}/exarch" "${install_dir}/exarch"
echo "Installed ${install_dir}/exarch"

if [ "$os" = "Darwin" ]; then
    codesign -s - "${install_dir}/exarch" 2>/dev/null || true
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

echo ""
echo "exarch is ready.  The ral shell it runs on is built in — no separate install."
echo "Set a provider key in your environment, then run exarch from a project:"
echo "  ANTHROPIC_API_KEY=… exarch"
