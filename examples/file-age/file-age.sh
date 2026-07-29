#!/usr/bin/env bash
# Report the age in days of every file in a directory.
set -euo pipefail

dir="$1"
now="$(date +%s)"

if stat -c %Y . >/dev/null 2>&1; then
    mtime_of() { stat -c %Y -- "$1"; }
else
    mtime_of() { stat -f %m -- "$1"; }
fi

while IFS= read -r -d '' f; do
    mtime="$(mtime_of "$f")"
    printf '%d\t%s\n' "$(( (now - mtime) / 86400 ))" "$f"
done < <(find "$dir" -maxdepth 1 -type f -print0 | sort -z)
