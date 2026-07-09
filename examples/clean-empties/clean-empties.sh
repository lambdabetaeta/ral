#!/usr/bin/env bash
# Delete zero-byte files under a directory, reporting each removed.
set -euo pipefail

root="$1"

for f in $(find "$root" -type f -empty); do
    echo "removing $f"
    rm "$f"
done
