#!/usr/bin/env bash
# Extract a nested JSON field by dotted path: json-get config.json db.host
set -euo pipefail

jq ".$2" "$1"
