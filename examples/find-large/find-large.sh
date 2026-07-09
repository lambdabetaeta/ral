#!/usr/bin/env bash
# List the 10 largest files under a directory, biggest first.
set -euo pipefail

root="${1:-.}"

find "$root" -type f -printf '%s\t%p\n' | sort -rn | head
