#!/usr/bin/env bash
# Collapse runs of blank lines into a single blank line.
set -euo pipefail

cat -s "$@"
