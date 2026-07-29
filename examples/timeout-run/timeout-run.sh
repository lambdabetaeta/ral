#!/usr/bin/env bash
# Run a project's test harness under a wall-clock deadline.
set -euo pipefail

secs="$1"
dir="$2"

cd -- "$dir"

rc=0
timeout --kill-after=5 -- "$secs" ./run-tests || rc=$?

if ((rc == 0)); then
    echo "tests passed"
elif ((rc == 124)); then
    echo "tests exceeded the ${secs}s deadline" >&2
    exit 1
else
    echo "tests failed (exit $rc)" >&2
    exit 1
fi
