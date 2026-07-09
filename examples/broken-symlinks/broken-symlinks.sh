#!/usr/bin/env bash
# List symlinks whose target does not exist.
set -euo pipefail

root="$1"

for l in $(find "$root" -type l); do
    if [ ! -e "$l" ]; then
        echo "$l -> $(readlink "$l")"
    fi
done
