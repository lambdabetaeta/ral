#!/usr/bin/env bash
# Draw an ASCII bar chart from a file of "category count" lines.
set -euo pipefail

max=1
while read -r _ count; do
    (( count > max )) && max=$count
done < "$1"

while read -r cat count; do
    width=$(( count * 40 / max ))
    bar=$(printf '%*s' "$width" '' | tr ' ' '#')
    printf '%s\t%s %d\n' "$cat" "$bar" "$count"
done < "$1"
