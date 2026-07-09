#!/usr/bin/env bash
# Split a text file into chunks of N lines each.
set -euo pipefail
src=$1; n=$2
split -l "$n" -d "$src" "$src.part"
