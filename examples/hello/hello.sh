#!/usr/bin/env bash
# Greet each argument, defaulting to "world".
set -euo pipefail

if [ "$#" -eq 0 ]; then
    set -- world
fi

for name in "$@"; do
    echo "hello, $name"
done
