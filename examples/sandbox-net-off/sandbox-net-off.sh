#!/usr/bin/env bash
# Run a build step with the network denied, falling back to vendored deps.
set -euo pipefail

deps="$(mktemp)"
trap 'rm -f "$deps"' EXIT

echo 'building offline (network denied)...'
# Drop into a fresh network namespace with no interfaces (needs unshare + userns).
if unshare -rn bash -c 'curl -s https://registry.example.com/latest' > "$deps" 2>/dev/null; then
    echo 'fetched remote deps'
else
    echo 'network denied — falling back to vendored deps'
fi
echo "compiled at $(date +%s)"
