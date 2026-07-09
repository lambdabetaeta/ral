#!/usr/bin/env bash
# Turn a CSV with a header row into a JSON array of objects.
set -euo pipefail

jq -R -s -c '
  split("\n") | map(select(length > 0)) |
  (.[0] | split(",")) as $h |
  .[1:] | map(split(",") | [$h, .] | transpose | map({(.[0]): .[1]}) | add)
' "$1"
