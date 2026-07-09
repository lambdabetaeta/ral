#!/usr/bin/env bash
# Keep JSON array elements whose field equals a value: filter users.json role admin
set -euo pipefail

jq "[.[] | select(.$2 == \"$3\")]" "$1"
