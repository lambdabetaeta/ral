#!/usr/bin/env bash
# A quick reading of memory and load pressure on macOS.
set -euo pipefail

uptime
sysctl vm.swapusage
memory_pressure | tail -1
ps -Aco pid,pmem,rss,comm -m | head
sysctl -n hw.memsize
