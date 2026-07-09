#!/usr/bin/env bash
# Copy every *.EXT under a source tree into a flat destination directory.
set -euo pipefail

src="$1"
dst="$2"
ext="$3"
mkdir -p "$dst"

cp $(find "$src" -name "*.$ext") "$dst"
