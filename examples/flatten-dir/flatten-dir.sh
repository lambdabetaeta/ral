#!/usr/bin/env bash
# Copy every nested file into one flat output dir, numbering basename clashes.
set -euo pipefail

src="$1"
dst="$2"
mkdir -p -- "$dst"

copied=0
renamed=0
while IFS= read -r -d '' p; do
    name="${p##*/}"
    dest="$dst/$name"
    n=0
    while [ -e "$dest" ]; do
        n=$((n + 1))
        dest="$dst/$n-$name"
    done
    cp -- "$p" "$dest"
    copied=$((copied + 1))
    if [ "$n" -gt 0 ]; then renamed=$((renamed + 1)); fi
done < <(find "$src" -type f -print0 | LC_ALL=C sort -z)

echo "flattened $copied files into $dst ($renamed renamed)"
