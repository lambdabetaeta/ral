#!/usr/bin/env bash
# Group files with identical content (by sha256) and print duplicate sets.
set -euo pipefail

root="$1"

find "$root" -type f -exec sha256sum {} + \
    | sort \
    | awk '{
        hash = $1
        path = $2
        if (hash == prev) {
            if (!shown) { print prevpath; shown = 1 }
            print path
        } else {
            shown = 0
        }
        prev = hash
        prevpath = path
    }'
