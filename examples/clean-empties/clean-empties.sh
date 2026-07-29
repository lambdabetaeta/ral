#!/usr/bin/env bash
# Delete zero-byte files under a directory, skipping dotted entries;
# report each removal, then the folders that were touched.
set -euo pipefail

root="$(cd -- "$1" && pwd -P)"

list="$(mktemp)"
trap 'rm -f -- "$list"' EXIT
find "$root" -mindepth 1 -name '.*' -prune -o -type f -empty -print0 \
    | LC_ALL=C sort -z > "$list"

count=0
folders=()
while IFS= read -r -d '' f; do
    printf 'removing %s\n' "$f"
    rm -- "$f"
    count=$((count + 1))
    folders+=("$(dirname -- "$f")")
done < "$list"

touched=()
if ((count > 0)); then
    while IFS= read -r -d '' d; do
        touched+=("$d")
    done < <(printf '%s\0' "${folders[@]}" | LC_ALL=C sort -zu)
fi

printf 'removed %d empty files in %d folders\n' "$count" "${#touched[@]}"
for d in ${touched[@]+"${touched[@]}"}; do
    printf '  %s\n' "$d"
done
