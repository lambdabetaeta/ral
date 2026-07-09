#!/usr/bin/env bash
# Increment the patch component of a semver VERSION file, in place.
set -euo pipefail

path="$1"

IFS=. read -r major minor patch < "$path"
new="$major.$minor.$((patch + 1))"
echo "$new" > "$path"
echo "$new"
