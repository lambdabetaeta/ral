#!/usr/bin/env bash
# Count CSV rows grouped by a column value (column given by number).
set -euo pipefail

file=$1 col=$2
tail -n +2 "$file" | cut -d, -f"$col" | sort | uniq -c
