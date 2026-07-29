#!/usr/bin/env bash
# Load a partial JSON config, reject unknown keys, fill the rest from defaults.
set -euo pipefail

path="$1"
defaults='{"host":"localhost","port":8080,"tls":{"enabled":false,"ca":""}}'

unknown="$(jq -rce --argjson d "$defaults" '(keys - ($d | keys)) | join(", ")' -- "$path")"
if [ -n "$unknown" ]; then
    printf 'unknown config keys: %s\n' "$unknown" >&2
    exit 1
fi

jq -ce --argjson d "$defaults" '$d + .' -- "$path"
