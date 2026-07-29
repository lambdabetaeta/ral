#!/usr/bin/env bash
# Print the N CSV rows with the largest value in a named column.
set -euo pipefail

file="$1" name="$2" n="$3"

idx="$(awk -F, -v want="$name" 'NR == 1 { for (i = 1; i <= NF; i++) if ($i == want) { print i; exit } }' "$file")"
if [ -z "$idx" ]; then
    echo "$file: no column named $name" >&2
    exit 1
fi

head -n 1 -- "$file"
tail -n +2 -- "$file" | sort -t, -k"$idx","$idx" -rn | head -n "$n"
