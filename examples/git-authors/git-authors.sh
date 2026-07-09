#!/usr/bin/env bash
# Count commits per author in a git repository.
set -euo pipefail

repo="$1"

git -C "$repo" shortlog -sn
