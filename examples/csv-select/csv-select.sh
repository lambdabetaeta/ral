#!/usr/bin/env bash
# Project the named CSV columns, in the order requested.
set -euo pipefail

file=$1
shift

IFS=, read -r -a header < "$file"

fields=()
for want in "$@"; do
    found=
    for i in "${!header[@]}"; do
        if [ "${header[$i]}" = "$want" ]; then found=$((i + 1)); break; fi
    done
    if [ -z "$found" ]; then
        printf 'no such column: %s\n' "$want" >&2
        exit 1
    fi
    fields+=("$found")
done

list=$(IFS=,; printf '%s' "${fields[*]}")
cut -d, -f"$list" -- "$file"
