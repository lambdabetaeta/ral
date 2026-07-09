#!/usr/bin/env bash
# Keep the N newest backups in a directory; delete the rest.
set -euo pipefail

dir="$1"
keep="$2"

cd "$dir"
ls -t | tail -n +$((keep + 1)) | while read -r f; do
    echo "removing $f"
    rm "$f"
done
