#!/usr/bin/env bash
# Print mean, median, min, max of a numeric CSV column (given by number).
set -euo pipefail

file=$1 col=$2
tail -n +2 "$file" | cut -d, -f"$col" | sort -n | awk '
{ a[NR] = $1; s += $1 }
END {
    printf "mean\t%s\n", s / NR
    printf "median\t%s\n", (NR % 2) ? a[(NR + 1) / 2] : (a[NR / 2] + a[NR / 2 + 1]) / 2
    printf "min\t%s\n", a[1]
    printf "max\t%s\n", a[NR]
}'
