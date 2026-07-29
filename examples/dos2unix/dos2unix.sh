#!/usr/bin/env bash
# Strip carriage returns from the named files, rewriting each in place.
set -euo pipefail
export LC_ALL=C

tmp=
trap 'if [[ -n $tmp ]]; then rm -f -- "$tmp"; fi' EXIT

for arg in "$@"; do
    path="$(readlink -f -- "$arg")"
    tmp="$(mktemp -- "$path.XXXXXX")"
    cp -p -- "$path" "$tmp"
    tr -d '\r' < "$path" > "$tmp"
    removed=$(( $(wc -c < "$path") - $(wc -c < "$tmp") ))
    if (( removed == 0 )); then
        rm -f -- "$tmp"
        tmp=
        continue
    fi
    mv -- "$tmp" "$path"
    tmp=
    printf '%s: %d CRs removed\n' "$arg" "$removed"
done
