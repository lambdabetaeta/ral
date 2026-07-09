#!/usr/bin/env bash
# Print lines present in both files.
set -euo pipefail

comm -12 <(sort "$1") <(sort "$2")
