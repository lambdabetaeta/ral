#!/usr/bin/env bash
# Print mean, median (middle value), min and max of a named CSV column.
set -euo pipefail

file="$1"
field="$2"

idx="$(awk -F, -v want="$field" \
    'NR == 1 { for (i = 1; i <= NF; i++) if ($i == want) { print i; exit } }' "$file")"
if [ -z "$idx" ]; then
    printf '%s has no field %s\n' "$file" "$field" >&2
    exit 1
fi

tail -n +2 -- "$file" | cut -d, -f"$idx" | LC_ALL=C sort -n | awk '
    { a[NR] = $1; s += $1 }
    END {
        if (NR == 0) { print "no data rows" > "/dev/stderr"; exit 1 }
        printf "mean\t%s\n", s / NR
        printf "median\t%s\n", a[int((NR + 1) / 2)]
        printf "min\t%s\n", a[1]
        printf "max\t%s\n", a[NR]
    }'
