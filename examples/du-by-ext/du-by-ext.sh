#!/usr/bin/env bash
# Sum file sizes under a directory, grouped by extension; biggest total first.
set -euo pipefail

root="${1:-.}"

find "$root" -type f -printf '%s\t%f\n' \
    | awk -F'\t' '{n=split($2,a,".");s[a[n]]+=$1} END {for (k in s) print s[k],k}' \
    | sort -rn
