#!/usr/bin/env bash
# A memory and load vitals reading for macOS.
set -euo pipefail

ram_gb=$(( $(sysctl -n hw.memsize) / 1073741824 ))

read -r _ m1 m5 m15 _ <<<"$(sysctl -n vm.loadavg)"
read -r _ _ _ total _ _ used _ <<<"$(sysctl vm.swapusage)"
swap_total="${total%M}"
swap_used="${used%M}"

free_pct="$(memory_pressure | tail -n 1 | sed -E 's/^.*: ([0-9]+)%$/\1/')"

hogs="$(ps -Aco pid,pmem,rss,comm -m | head -n 7 | tail -n +2 | awk '{
    name = $4
    for (i = 5; i <= NF; i++) name = name " " $i
    printf "hog %s %d %s\n", $2, $3 / 1024, name
}')"

printf 'ram-gb %s\n' "$ram_gb"
printf 'load %s %s %s\n' "$m1" "$m5" "$m15"
printf 'swap-mb %s used of %s\n' "$swap_used" "$swap_total"
printf 'free-pct %s\n' "$free_pct"
printf '%s\n' "$hogs"
