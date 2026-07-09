#!/usr/bin/env bash
# Hard-wrap lines longer than WIDTH at word boundaries.
set -euo pipefail

width=$1
file=$2

fold -s -w "$width" "$file"
