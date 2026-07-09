#!/usr/bin/env bash
# Top N immediate subdirectories by total size, largest first.
set -euo pipefail
root=$1; n=$2
du -sk "$root"/*/ | sort -rn | head -n "$n"
