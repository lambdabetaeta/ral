#!/usr/bin/env bash
# Rename files in a directory to a zero-padded numeric sequence.
set -euo pipefail
dir=$1; prefix=$2
i=1
for f in "$dir"/*; do
    [ -f "$f" ] || continue
    ext=${f##*.}
    printf -v seq '%04d' "$i"
    mv -- "$f" "$dir/$prefix-$seq.$ext"
    i=$((i + 1))
done
