#!/usr/bin/env bash
# Report files added, removed, and changed between two directory trees.
set -euo pipefail

a="$1"; b="$2"

diff -rq "$a" "$b" | while read -r line; do
    case "$line" in
        "Only in $a"*)  echo "- ${line#Only in $a: }" ;;
        "Only in $b"*)  echo "+ ${line#Only in $b: }" ;;
        Files\ *\ differ) rel="${line#Files $a/}"; echo "~ ${rel%% and *}" ;;
    esac
done
