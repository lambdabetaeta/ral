#!/usr/bin/env bash
# Strip full-line # comments and blank lines from a config file.
set -euo pipefail

grep -v '^#' "$1"
