#!/usr/bin/env bash
# List directories that contain no files anywhere beneath them.
set -euo pipefail

root="$1"

while IFS= read -r -d '' d; do
    if [ -z "$(find "$d" -type f -print -quit)" ]; then
        printf '%s\n' "$d"
    fi
done < <(find "$root" -type d -print0)
