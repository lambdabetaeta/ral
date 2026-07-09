#!/usr/bin/env bash
# List directories that contain no files anywhere beneath them.
set -euo pipefail

root="$1"

find "$root" -type d | while read -r d; do
    if [ -z "$(find "$d" -type f -print -quit)" ]; then
        echo "$d"
    fi
done
