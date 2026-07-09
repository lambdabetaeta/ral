#!/usr/bin/env bash
# Report filesystems whose usage exceeds a threshold percent.
set -euo pipefail

threshold="$1"

df -P | awk -v t="$threshold" 'NR > 1 && (0 + $5) > t {
    print $5 "\t" $6
}'
