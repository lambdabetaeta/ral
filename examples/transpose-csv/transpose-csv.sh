#!/usr/bin/env bash
# Swap rows and columns of a simple CSV.
set -euo pipefail

awk -F, '
    { for (i = 1; i <= NF; i++) a[i, NR] = $i; if (NF > cols) cols = NF }
    END {
        for (i = 1; i <= cols; i++) {
            sep = ""
            for (j = 1; j <= NR; j++) { printf "%s%s", sep, a[i, j]; sep = "," }
            print ""
        }
    }
' "$1"
