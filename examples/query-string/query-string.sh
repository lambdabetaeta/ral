#!/usr/bin/env bash
# Parse a URL query string into decoded key=value lines.
set -euo pipefail

query="$1"
IFS='&' read -ra pairs <<< "$query"
for p in "${pairs[@]}"; do
    IFS='=' read -ra kv <<< "$p"
    key="${kv[0]}"
    val="${kv[1]:-}"
    printf '%s=%b\n' "$key" "${val//%/\\x}"
done
