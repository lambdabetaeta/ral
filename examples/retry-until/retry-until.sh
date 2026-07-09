#!/usr/bin/env bash
# Fetch a URL, retrying up to five times with a pause between attempts.
set -euo pipefail
for attempt in {1..5}; do
    if curl -fsS --max-time 10 "$1"; then
        exit 0
    fi
    sleep 2
done
echo "giving up after 5 attempts" >&2
exit 1
