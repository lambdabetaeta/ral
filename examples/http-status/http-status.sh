#!/usr/bin/env bash
# Report the HTTP status code for each URL, concurrently.
set -euo pipefail
for u in "$@"; do
    curl -s -o /dev/null -w "%{http_code} $u"$'\n' --max-time 15 "$u" &
done
wait
