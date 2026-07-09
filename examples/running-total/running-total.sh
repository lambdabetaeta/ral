#!/usr/bin/env bash
# Append a cumulative-sum column to a CSV (source column given by number).
set -euo pipefail

file=$1 col=$2
awk -F, -v c="$col" 'NR == 1 { print $0 ",cumulative"; next } { s += $c; print $0 "," s }' "$file"
