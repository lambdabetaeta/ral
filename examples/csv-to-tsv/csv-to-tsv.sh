#!/usr/bin/env bash
# Convert a comma-separated file to tab-separated.
set -euo pipefail

sed 's/,/\t/g' "$1"
