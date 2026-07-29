#!/usr/bin/env bash
# Print the top N most frequent words in a text file, most frequent first.
set -euo pipefail

tr -cs '[:alpha:]' '\n' < "$1" \
    | tr '[:upper:]' '[:lower:]' \
    | sort \
    | uniq -c \
    | sort -rn \
    | head -n "$2"
