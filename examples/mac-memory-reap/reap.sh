#!/usr/bin/env bash
# Kill named memory hogs and eyeball whether it helped, on macOS.
set -euo pipefail

pkill -i zulip; sleep 1; pgrep -il zulip
pkill -i "Microsoft Edge"
pkill -i discord

sysctl vm.swapusage
uptime
