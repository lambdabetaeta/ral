#!/usr/bin/env bash
# Hash every file under a directory in parallel.
set -euo pipefail

root="${1:-.}"
jobs="${2:-8}"

find "$root" -type f -print0 | xargs -0 -P "$jobs" sha256sum
