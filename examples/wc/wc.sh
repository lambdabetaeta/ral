#!/usr/bin/env bash
# Count lines, words, and bytes for each file argument.
set -euo pipefail

if [ "$#" -eq 0 ]; then
    echo 'usage: wc.sh FILE...' >&2
    exit 2
fi

for f in "$@"; do
    read -r lines words bytes _ < <(wc -l -w -c "$f")
    printf '%s\t%s\t%s\t%s\n' "$lines" "$words" "$bytes" "$f"
done
