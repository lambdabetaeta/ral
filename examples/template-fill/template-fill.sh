#!/usr/bin/env bash
# Fill ${VAR} placeholders in a template file from the environment.
set -euo pipefail

tpl="$(cat -- "$1")"
placeholders="$(grep -o -e '\${[A-Z_][A-Z0-9_]*}' -- "$1" | sort -u || true)"

out="$tpl"
while IFS= read -r ph; do
    [ -n "$ph" ] || continue
    name="${ph:2:${#ph}-3}"
    out="${out//"$ph"/${!name-}}"
done <<<"$placeholders"

printf '%s\n' "$out"
