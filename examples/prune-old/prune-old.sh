#!/usr/bin/env bash
# Delete files older than N days under a directory, reporting each.
set -euo pipefail
dir=$1; days=$2
for f in $(find "$dir" -type f -mtime +"$days"); do
    rm -- "$f"
    echo "removed $f"
done
