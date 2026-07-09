#!/usr/bin/env bash
# Flatten a JSON array of flat objects into CSV with a header row.
set -euo pipefail

jq -r '(.[0] | keys_unsorted) as $k | ($k | @csv), (.[] | [.[$k[]]] | @csv)' "$1"
