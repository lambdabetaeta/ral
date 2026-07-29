#!/usr/bin/env bash
# Fetch a URL, retrying up to five times with a pause between attempts.
set -euo pipefail

url="$1"

for attempt in 1 2 3 4 5; do
    if curl -fsS --max-time 10 -- "$url"; then
        exit 0
    fi
    if ((attempt < 5)); then
        sleep 2
    fi
done

echo "giving up after 5 attempts" >&2
exit 1
