#!/usr/bin/env bash
# Print the N CSV rows with the largest value in a column (given by number).
set -euo pipefail

file=$1 col=$2 n=$3
head -n1 "$file"
tail -n +2 "$file" | sort -t, -k"$col" -rn | head -n "$n"
