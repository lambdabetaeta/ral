#!/usr/bin/env bash
# Count files grouped by extension under a tree, biggest group first.
set -euo pipefail

root="$1"

find "$root" -type f -printf '%f\n' \
    | awk -F. '{ print (NF > 1 ? $NF : "(none)") }' \
    | sort \
    | uniq -c \
    | sort -rn
