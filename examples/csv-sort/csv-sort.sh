#!/usr/bin/env bash
# Sort a CSV by a numeric column (given by column number), descending.
set -euo pipefail

file=$1 col=$2
{
  IFS= read -r header
  printf '%s\n' "$header"
  sort -t, -k"$col","$col" -rn
} < "$file"
