#!/usr/bin/env bash
# Wait for a TCP port to accept connections, giving up after a deadline.
set -euo pipefail

host="$1"
port="$2"
secs="$3"

deadline=$((SECONDS + secs))
while ((SECONDS < deadline)); do
    if nc -z -G 1 -w 1 -- "$host" "$port" 2>/dev/null; then
        echo "$host:$port is up"
        exit 0
    fi
    ((SECONDS < deadline)) || break
    sleep 1
done
printf '%s:%s never came up within %ss\n' "$host" "$port" "$secs" >&2
exit 1
