#!/usr/bin/env bash
# Report the age in days of each file in a directory.
set -euo pipefail
dir=$1
now=$(date +%s)
for f in "$dir"/*; do
    [ -f "$f" ] || continue
    mtime=$(stat -c %Y "$f")
    echo -e "$(( (now - mtime) / 86400 ))\t$f"
done
