#!/usr/bin/env bash
# Print the top N most frequent words in a text file, most frequent first.
set -euo pipefail

tr -cs 'A-Za-z' '\n' < "$1" \
    | tr 'A-Z' 'a-z' \
    | sort \
    | uniq -c \
    | sort -rn \
    | head -n "$2"
