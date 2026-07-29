#!/usr/bin/env bash
# Print the named columns of a CSV file as an aligned table.
set -euo pipefail

file="$1"
shift

awk -F, -v want="$*" '
    NR == 1 {
        for (i = 1; i <= NF; i++) where[$i] = i
        n = split(want, names, " ")
        line = names[1]
        for (j = 2; j <= n; j++) line = line "\t" names[j]
        print line
        next
    }
    {
        line = $(where[names[1]])
        for (j = 2; j <= n; j++) line = line "\t" $(where[names[j]])
        print line
    }
' < "$file" | column -t -s "$(printf '\t')"
