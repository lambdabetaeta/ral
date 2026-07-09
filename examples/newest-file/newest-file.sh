#!/usr/bin/env bash
# Print the most recently modified file under a directory tree.
set -euo pipefail

root="$1"

find "$root" -type f -printf '%T@ %p\n' \
    | sort -n \
    | tail -1 \
    | cut -d' ' -f2-
