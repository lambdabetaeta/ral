#!/usr/bin/env bash
# List local branches already merged into main (candidates to delete).
set -euo pipefail

repo="$1"

cd "$repo"
for b in $(git branch --merged main); do
    [ "$b" = "main" ] && continue
    echo "$b"
done
