#!/usr/bin/env bash
# Deduplicate PATH, keeping the first occurrence of each directory.
set -euo pipefail

echo "$PATH" | awk -v RS=: '!seen[$0]++ {
    out = out (out ? ":" : "") $0
} END { print out }'
