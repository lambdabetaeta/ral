#!/usr/bin/env bash
# Multiplex tail -f across several logs, prefixing each line with its source.
set -euo pipefail

if [ "$#" -eq 0 ]; then
    echo 'usage: tail-multi.sh FILE...' >&2
    exit 2
fi

pids=()
for f in "$@"; do
    tail -f "$f" | sed "s|^|[$(basename "$f")] |" &
    pids+=($!)
done

trap 'kill "${pids[@]}" 2>/dev/null' EXIT
wait
