#!/usr/bin/env bash
# List the top-level keys of each JSON file, and the keys it lacks
# relative to the others.
set -euo pipefail

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

keyfiles=()
i=0
for f in "$@"; do
    jq -r 'keys[]' -- "$f" | LC_ALL=C sort > "$tmp/$i"
    keyfiles+=("$tmp/$i")
    i=$((i + 1))
done

LC_ALL=C sort -u -- "${keyfiles[@]}" > "$tmp/all"

i=0
for f in "$@"; do
    printf '%s\n' "$f"
    while IFS= read -r k; do
        printf '  %s\n' "$k"
    done < "$tmp/$i"
    missing="$(LC_ALL=C comm -23 "$tmp/all" "$tmp/$i" | paste -sd, -)"
    if [ -n "$missing" ]; then
        printf '  missing: %s\n' "$missing"
    fi
    i=$((i + 1))
done
