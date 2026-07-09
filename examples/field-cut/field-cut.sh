#!/usr/bin/env bash
# Extract columns (1-based) from a delimited file, in the given order.
set -euo pipefail

file=$1
delim=$2
shift 2

fields=$(IFS=,; echo "$*")
cut -d"$delim" -f"$fields" "$file"
