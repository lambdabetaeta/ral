#!/usr/bin/env bash
# Hash every file under a directory in parallel, as a sorted manifest.
set -euo pipefail

root="${1:-.}"
jobs="${2:-8}"

find "$root" -type f -print0 \
    | xargs -0 -P "$jobs" sha256sum -- \
    | LC_ALL=C sort -k2
