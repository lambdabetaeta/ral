#!/usr/bin/env bash
# Verify files against a sha256sums-format manifest; report OK/FAILED per file.
set -euo pipefail

manifest="$1"
base="$(dirname "$manifest")"

while read -r want file; do
    got="$(sha256sum "$base/$file" | cut -d' ' -f1)"
    if [ "$want" = "$got" ]; then
        echo "OK  $file"
    else
        echo "FAILED  $file"
    fi
done < "$manifest"
