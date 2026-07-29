#!/usr/bin/env bash
# Keep the CSV rows whose named column exceeds a threshold.
set -euo pipefail

if [ "$#" -ne 3 ]; then
    echo "usage: ${0##*/} FILE COLUMN THRESHOLD" >&2
    exit 2
fi

file=$1 column=$2 threshold=$3

awk -F, -v want="$column" -v t="$threshold" '
    NR == 1 {
        for (i = 1; i <= NF; i++) { col[$i] = i }
        if (!(want in col)) {
            print "no such column: " want > "/dev/stderr"
            exit 1
        }
        c = col[want]
        print
        next
    }
    $c > t
' < "$file"
