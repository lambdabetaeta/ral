#!/usr/bin/env bash
# Generate a random password of the requested length.
set -euo pipefail

len="$1"
LC_ALL=C tr -dc 'A-Za-z0-9' < /dev/urandom | head -c "$len"; echo
