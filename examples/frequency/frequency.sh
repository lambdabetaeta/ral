#!/usr/bin/env bash
# Count how often each distinct stdin line occurs, most frequent first.
set -euo pipefail

sort | uniq -c | sort -rn |
    awk '{ n = $1; sub(/^[[:blank:]]*[0-9]+[[:blank:]]/, ""); print n "\t" $0 }'
