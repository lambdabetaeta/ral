#!/usr/bin/env bash
# Copy a source file to a destination only if the source is newer, and say which.
set -euo pipefail

src="$1"
dst="$2"

if [[ "$src" -nt "$dst" ]]; then
    cp -- "$src" "$dst"
    echo "copied $src -> $dst ($(wc -c <"$src" | tr -d ' ') bytes)"
else
    echo "up to date: $dst"
fi
