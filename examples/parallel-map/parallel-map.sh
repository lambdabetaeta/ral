#!/usr/bin/env bash
# Gzip every given file, four workers at a time.
set -euo pipefail
printf '%s\n' "$@" | xargs -P4 -I{} gzip -kf {}
