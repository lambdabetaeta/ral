#!/usr/bin/env bash
# Report how many lines were added vs removed between two files.
set -euo pipefail

d=$(diff "$1" "$2")
echo "added:   $(echo "$d" | grep -c '^>')"
echo "removed: $(echo "$d" | grep -c '^<')"
