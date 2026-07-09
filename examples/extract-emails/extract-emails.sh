#!/usr/bin/env bash
# Pull every distinct email address out of a text file.
set -euo pipefail

file=$1

grep -oE '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' "$file" | sort -u
