#!/usr/bin/env bash
# Print a file's lines in reverse order.
set -euo pipefail

file=$1

tac "$file"
