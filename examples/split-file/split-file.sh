#!/usr/bin/env bash
# Split a text file into chunks of N lines each, naming the chunks written.
set -euo pipefail
shopt -s nullglob

input="$1"
n="$2"

split -l "$n" -d -- "$input" "$input.part"

for part in "$input".part*; do
    echo "$part"
done
