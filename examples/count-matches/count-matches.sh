#!/usr/bin/env bash
# Count lines matching a pattern in each file, skipping files with none.
set -euo pipefail

pattern=$1
shift

total=0
for f in "$@"; do
    n=$(grep -c -- "$pattern" "$f") || n=0
    if (( n > 0 )); then
        echo "$f:$n"
        total=$(( total + n ))
    fi
done

echo "total:$total"
