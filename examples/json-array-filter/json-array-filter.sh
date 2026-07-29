#!/usr/bin/env bash
# Keep JSON array elements whose field equals a value: filter users.json role admin
set -euo pipefail

file="$1"
field="$2"
want="$3"

jq --arg want "$want" "[.[] | select(.$field == \$want)]" -- "$file"
