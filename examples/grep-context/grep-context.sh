#!/usr/bin/env bash
# Print N lines of context around each match of a pattern in a file.
set -euo pipefail

pattern=$1
n=$2
file=$3

grep -C "$n" "$pattern" "$file"
