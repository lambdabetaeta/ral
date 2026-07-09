#!/usr/bin/env bash
# Print lines in the first file that are absent from the second.
set -euo pipefail

comm -23 <(sort "$1") <(sort "$2")
