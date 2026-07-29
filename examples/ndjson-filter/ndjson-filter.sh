#!/usr/bin/env bash
# Keep JSONL records whose field equals a value: ndjson-filter.sh users.jsonl role admin
set -euo pipefail

if [ "$#" -ne 3 ]; then
    printf 'usage: %s <file.jsonl> <field> <value>\n' "${0##*/}" >&2
    exit 2
fi
file=$1
field=$2
want=$3

if [ ! -f "$file" ] || [ ! -r "$file" ]; then
    printf '%s: %s: not a readable regular file\n' "${0##*/}" "$file" >&2
    exit 1
fi

if ! jq -e -s 'all(type == "object")' -- "$file" > /dev/null; then
    printf '%s: %s: not valid JSON Lines of objects\n' "${0##*/}" "$file" >&2
    exit 1
fi

jq -c --arg field "$field" --arg want "$want" \
    'select(has($field) and (.[$field] | tostring) == $want)' -- "$file"
