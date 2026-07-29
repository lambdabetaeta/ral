#!/usr/bin/env bash
# Report symlinks whose target is missing, as CSV, minus the ones a waiver CSV lists.
set -euo pipefail

root="$1"
waivers="$2"

waived=()
{
    read -r _
    while IFS=, read -r link _; do
        waived+=("$link")
    done
} < "$waivers"

rows=()
while IFS= read -r -d '' link; do
    if [ -e "$link" ]; then continue; fi
    for w in ${waived[@]+"${waived[@]}"}; do
        if [ "$w" = "$link" ]; then continue 2; fi
    done
    rows+=("$link,$(readlink -- "$link")")
done < <(find "$root" -path '*/.*' -prune -o -type l -print0 | LC_ALL=C sort -z)

if [ "${#rows[@]}" -gt 0 ]; then
    printf 'link,target\n'
    printf '%s\n' "${rows[@]}"
fi
