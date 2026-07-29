#!/usr/bin/env bash
# Slugify every title in a file; report slugs claimed by more than one title.
set -euo pipefail

path="$1"

slugify() {
    printf '%s\n' "$1" \
        | tr '[:upper:]' '[:lower:]' \
        | sed -E 's/[^a-z0-9]+/-/g; s/^-+|-+$//g'
}

slugs=()
while IFS= read -r title; do
    [ -n "$title" ] || continue
    slug="$(slugify "$title")"
    printf '%s\n' "$slug"
    slugs+=("$slug")
done < "$path"

if [ "${#slugs[@]}" -gt 0 ]; then
    printf '%s\n' "${slugs[@]}" | sort | uniq -d | while IFS= read -r s; do
        printf 'clash: %s\n' "$s"
    done
fi
