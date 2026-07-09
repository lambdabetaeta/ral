#!/usr/bin/env bash
# Load a partial JSON config and fill missing keys from defaults.
set -euo pipefail

defaults='{"host":"localhost","port":8080,"verbose":false}'
jq -s '.[0] * .[1]' <(printf '%s' "$defaults") "$1"
