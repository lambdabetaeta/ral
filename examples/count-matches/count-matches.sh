#!/usr/bin/env bash
# Count lines matching a pattern in each file.
set -euo pipefail

pattern=$1
shift

for f in "$@"; do
    echo "$f:$(grep -c "$pattern" "$f")"
done
