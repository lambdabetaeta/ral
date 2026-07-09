#!/usr/bin/env bash
# Count non-blank lines of code grouped by extension, biggest first.
set -euo pipefail

root="$1"

find "$root" -type f | while read -r f; do
    ext="${f##*.}"
    n="$(grep -c . "$f" || true)"
    printf '%s %s\n' "$ext" "$n"
done | awk '{ s[$1] += $2 } END { for (k in s) print s[k], k }' | sort -rn
