#!/usr/bin/env bash
# Print stdin lines unique, preserving first-seen order.
set -euo pipefail

awk '!seen[$0]++'
