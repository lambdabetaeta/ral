#!/usr/bin/env bash
# Base64-encode a file and decode it back, verifying the round-trip.
set -euo pipefail

enc="$(mktemp)"; dec="$(mktemp)"
trap 'rm -f "$enc" "$dec"' EXIT

base64 "$1" > "$enc"
base64 -d "$enc" > "$dec"
if cmp -s "$1" "$dec"; then
    echo "round-trip ok: $(wc -l < "$enc") lines of base64"
else
    echo 'round-trip mismatch' >&2; exit 1
fi
