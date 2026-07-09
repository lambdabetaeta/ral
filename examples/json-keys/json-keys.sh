#!/usr/bin/env bash
# List the top-level keys of a JSON object, one per line.
set -euo pipefail

jq -r 'keys[]' "$1"
