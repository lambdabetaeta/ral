#!/usr/bin/env bash
# Inner-join two CSVs on a shared key column (given by number).
set -euo pipefail

left=$1 right=$2 key=$3
join -t, -1 "$key" -2 "$key" \
    <(sort -t, -k"$key" "$left") \
    <(sort -t, -k"$key" "$right")
