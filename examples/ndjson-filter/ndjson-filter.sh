#!/usr/bin/env bash
# Keep JSONL records whose field equals a value: filter users.jsonl role admin
set -euo pipefail

file="$1"; field="$2"; want="$3"

while IFS= read -r line; do
    printf '%s' "$line" | grep -q "\"$field\":\"$want\"" && printf '%s\n' "$line"
done < "$file"
