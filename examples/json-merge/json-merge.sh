#!/usr/bin/env bash
# Deep-merge two JSON config objects; the second wins on conflicts.
set -euo pipefail

jq -s '.[0] * .[1]' "$1" "$2"
