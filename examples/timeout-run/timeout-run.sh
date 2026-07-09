#!/usr/bin/env bash
# Run a build, killing it if it exceeds a wall-clock deadline.
set -euo pipefail
secs=$1; dir=$2
if timeout "$secs" make -C "$dir"; then
    exit 0
fi
echo "make -C $dir failed or timed out (exit $?)" >&2
exit 1
