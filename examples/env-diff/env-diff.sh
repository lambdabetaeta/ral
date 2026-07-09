#!/usr/bin/env bash
# Diff two `env`-dump files: show added, removed, and changed variables.
set -euo pipefail

old="$1"
new="$2"

comm -3 <(sort "$old") <(sort "$new") | while IFS= read -r line; do
    key="$(echo "$line" | cut -d= -f1)"
    val="$(echo "$line" | cut -d= -f2)"
    printf '%s\t%s\n' "$key" "$val"
done
