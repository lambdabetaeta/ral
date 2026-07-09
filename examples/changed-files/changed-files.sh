#!/usr/bin/env bash
# List files changed on the current branch versus a base branch.
set -euo pipefail

repo="$1"
base="$2"

cd "$repo"
for f in $(git diff --name-only "$base...HEAD"); do
    echo "$f"
done
