#!/usr/bin/env bash
# Verify files against a sha256sums-format manifest, resolving names
# relative to the manifest; report OK/FAILED per file.
set -euo pipefail

manifest="$1"
base="$(dirname -- "$manifest")"

total=0
bad=0
while read -r want file; do
    total=$((total + 1))
    if got="$(sha256sum -- "$base/$file" | cut -d' ' -f1)" && [ "$want" = "$got" ]; then
        echo "OK  $file"
    else
        echo "FAILED  $file"
        bad=$((bad + 1))
    fi
done < "$manifest"

if [ "$bad" -gt 0 ]; then
    echo "$bad of $total entries failed" >&2
    exit 1
fi

echo "$total entries verified"
