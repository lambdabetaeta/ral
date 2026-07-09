#!/usr/bin/env bash
# Report TODO/FIXME markers under a tree as file:line: text.
set -euo pipefail

root="$1"

grep -rn 'TODO\|FIXME' "$root"
