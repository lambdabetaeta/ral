#!/usr/bin/env bash
# Run make in several directories at once, waiting for all to finish.
set -euo pipefail
pids=()
for dir in "$@"; do
    make -C "$dir" &
    pids+=("$!")
done
for pid in "${pids[@]}"; do
    wait "$pid"
done
