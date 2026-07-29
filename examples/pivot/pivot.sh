#!/usr/bin/env bash
# Cross-tabulate a CSV: sum a value column over each (row key, column key) pair.
set -euo pipefail

file=$1 rowkey=$2 colkey=$3 valcol=$4

awk -F, -v rk="$rowkey" -v ck="$colkey" -v vc="$valcol" '
NR == 1 { for (i = 1; i <= NF; i++) idx[$i] = i; next }
{
    r = $idx[rk]; c = $idx[ck]
    if (!(r in rseen)) { rseen[r]; rowv[++nr] = r }
    if (!(c in cseen)) { cseen[c]; colv[++nc] = c }
    cell[r SUBSEP c] += $idx[vc]
}
END {
    line = rk
    for (j = 1; j <= nc; j++) line = line "\t" colv[j]
    print line
    for (i = 1; i <= nr; i++) {
        line = rowv[i]
        for (j = 1; j <= nc; j++) {
            k = rowv[i] SUBSEP colv[j]
            line = line "\t" (k in cell ? cell[k] : "")
        }
        print line
    }
}' "$file"
