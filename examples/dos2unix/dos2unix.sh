#!/usr/bin/env bash
# Normalize CRLF/CR line endings to LF, rewriting the file in place.
set -euo pipefail

sed -i 's/\r$//' "$1"
