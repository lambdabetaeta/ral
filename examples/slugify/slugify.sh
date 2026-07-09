#!/usr/bin/env bash
# Turn a title into a URL-safe lowercase hyphen slug.
set -euo pipefail

title="$1"
echo "$title" \
    | tr '[:upper:]' '[:lower:]' \
    | sed -E 's/[^a-z0-9]+/-/g; s/^-+|-+$//g'
