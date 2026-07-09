#!/usr/bin/env bash
# Bump a counter in a JSON state file and append a timestamp.
set -euo pipefail

path="$1"
stamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

tmp="$(mktemp)"
jq --arg t "$stamp" '.count += 1 | .history += [$t]' "$path" > "$tmp"
mv "$tmp" "$path"

echo "tick $(jq .count "$path") at $stamp"
