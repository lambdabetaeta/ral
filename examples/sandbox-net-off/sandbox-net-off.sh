#!/usr/bin/env bash
# Resolve a build's dependencies with the network denied, falling back to the
# vendored lockfile.
set -euo pipefail

registry="$1"
lockfile="$2"

work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

if ! unshare -rn true; then
    echo 'cannot deny the network: unshare -rn is unavailable here' >&2
    exit 1
fi

rc=0
unshare -rn curl -fsS -o "$work/fetched.csv" -- "$registry/latest" || rc=$?
case "$rc" in
    0)
        echo 'resolved from the registry'
        deps="$work/fetched.csv"
        ;;
    6 | 7)
        echo 'network denied — resolved from the vendored lockfile'
        deps="$lockfile"
        ;;
    *)
        printf 'resolve step failed with status %s\n' "$rc" >&2
        exit "$rc"
        ;;
esac

tail -n +2 -- "$deps" > "$work/body.csv"
LC_ALL=C sort -t, -k1,1 -- "$work/body.csv" > "$work/sorted.csv"

pinned=0
while IFS=, read -r name version; do
    printf '  %s %s\n' "$name" "$version"
    pinned=$(( pinned + 1 ))
done < "$work/sorted.csv"
printf '%s deps pinned\n' "$pinned"
