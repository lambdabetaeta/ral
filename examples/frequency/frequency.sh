#!/usr/bin/env bash
# Count how often each distinct stdin line occurs, most frequent first.
set -euo pipefail

sort | uniq -c | sort -rn
