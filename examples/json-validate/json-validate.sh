#!/usr/bin/env bash
# Validate a JSON file and canonicalise it to stdout (sorted keys).
set -euo pipefail

jq -S . "$1"
