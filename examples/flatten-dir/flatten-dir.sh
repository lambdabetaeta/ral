#!/usr/bin/env bash
# Copy all nested files into one flat output dir.
set -euo pipefail

src="$1"
dst="$2"
mkdir -p "$dst"

find "$src" -type f -exec cp {} "$dst" \;
