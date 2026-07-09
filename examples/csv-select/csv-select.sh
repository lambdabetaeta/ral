#!/usr/bin/env bash
# Keep the given CSV columns, addressed by number (e.g. 1,3).
set -euo pipefail

file=$1 cols=$2
cut -d, -f"$cols" "$file"
