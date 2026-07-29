#!/usr/bin/env bash
# Print a config file with blank lines and full-line # comments removed.
set -euo pipefail

grep -Ev '^[[:space:]]*(#|$)' -- "$1" || true
