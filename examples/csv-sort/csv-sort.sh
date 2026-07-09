#!/usr/bin/env bash
# Sort a CSV by a numeric column (given by number), descending.
set -euo pipefail

file=$1 col=$2
head -n1 "$file"
tail -n +2 "$file" | sort -t, -k"$col" -rn
