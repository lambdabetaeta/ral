#!/usr/bin/env bash
# Keep CSV rows whose numbered column exceeds a threshold.
set -euo pipefail

file=$1 col=$2 threshold=$3
awk -F, -v c="$col" -v t="$threshold" 'NR == 1 || $c > t' "$file"
