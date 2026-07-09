#!/usr/bin/env bash
# Strip leading and trailing whitespace from every line of a file.
set -euo pipefail

file=$1

while read -r line; do
    echo "$line"
done < "$file"
