#!/usr/bin/env bash
# Fetch a file from whichever mirror responds first.
set -euo pipefail
path=$1; shift
tmp=$(mktemp -d)
pids=()
for m in "$@"; do
    ( curl -fsSL --max-time 20 "$m/$path" > "$tmp/out" ) &
    pids+=("$!")
done
wait -n
kill "${pids[@]}" 2>/dev/null || true
cat "$tmp/out"
rm -rf "$tmp"
