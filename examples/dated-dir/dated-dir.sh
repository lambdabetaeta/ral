#!/usr/bin/env bash
# Create a directory named for today's date and report its path.
set -euo pipefail
base=$1
mkdir -p "$base/$(date +%F)"
echo "$base/$(date +%F)"
