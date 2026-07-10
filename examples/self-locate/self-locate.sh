#!/usr/bin/env bash
# Print the absolute path of a sibling file, regardless of where we're run.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"

echo "$ROOT/self-locate.sh"
