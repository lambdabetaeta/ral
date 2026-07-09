#!/usr/bin/env bash
# Group a CSV by one column, average a numeric column.
set -euo pipefail

path="$1"; group_col="$2"; value_col="$3"

awk -F',' -v g="$group_col" -v v="$value_col" '
    NR==1 { for (i=1;i<=NF;i++) col[$i]=i; next }
    { k=$col[g]; sum[k]+=$col[v]; n[k]++ }
    END { for (k in sum) printf "%s\t%s\t%d\n", k, sum[k]/n[k], n[k] }
' "$path"
