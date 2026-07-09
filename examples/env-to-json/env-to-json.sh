#!/usr/bin/env bash
# Emit selected environment variables as a JSON object: env-to-json HOME USER
set -euo pipefail

printf '{'
sep=""
for name in "$@"; do
    printf '%s"%s":"%s"' "$sep" "$name" "${!name}"
    sep=","
done
printf '}\n'
