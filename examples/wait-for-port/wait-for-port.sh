#!/usr/bin/env bash
# Wait for a TCP port to accept connections before continuing.
set -euo pipefail
host=$1; port=$2
until nc -z "$host" "$port" 2>/dev/null; do
    sleep 1
done
echo "$host:$port is up"
