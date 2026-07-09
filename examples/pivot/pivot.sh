#!/usr/bin/env bash
# Sum a value column grouped by a category column (both given by number).
set -euo pipefail

file=$1 cat=$2 val=$3
tail -n +2 "$file" | awk -F, -v c="$cat" -v v="$val" '
{ s[$c] += $v }
END { for (k in s) print k "\t" s[k] }'
