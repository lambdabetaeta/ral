#!/usr/bin/env bash
# Poll a JSON endpoint until a field reaches the target value.
set -euo pipefail
url=$1; field=$2; target=$3; tries=$4
for ((i=0; i<tries; i++)); do
    if curl -fsS --max-time 10 "$url" | grep -q "\"$field\": *\"$target\""; then
        echo "$field reached $target"
        exit 0
    fi
    sleep 2
done
echo "gave up waiting for $field=$target" >&2
exit 1
