#!/usr/bin/env bash
# Copy a source file to a destination only if the source is newer.
set -euo pipefail
src=$1; dst=$2
cp -u -- "$src" "$dst"
