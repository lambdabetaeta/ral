#!/usr/bin/env bash
# Parse a URL query string into decoded key=value lines.
set -euo pipefail

query="$1"

urldecode() {
    local s="${1//+/ }"
    printf '%b' "${s//%/\\x}"
}

IFS='&' read -ra pairs <<< "$query"
for p in "${pairs[@]}"; do
    IFS='=' read -r key val <<< "$p"
    printf '%s=%s\n' "$(urldecode "$key")" "$(urldecode "${val-}")"
done
