#!/usr/bin/env bash
# Parse a key=value config file, emit JSON to stdout.
set -euo pipefail

path="$1"

# shellcheck disable=SC1090
source "$path"

json='{}'
while IFS='=' read -r key _; do
    [[ "$key" =~ ^[[:space:]]*# || -z "$key" ]] && continue
    json="$(jq --arg k "$key" --arg v "${!key}" '. + {($k): $v}' <<<"$json")"
done < "$path"

echo "$json"
