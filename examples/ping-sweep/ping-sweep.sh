#!/usr/bin/env bash
# Ping a list of hosts in parallel and report which are up.
set -euo pipefail
for h in "$@"; do
    if ping -c1 -W1 "$h" >/dev/null 2>&1; then
        echo "up   $h"
    else
        echo "down $h"
    fi &
done
wait
