#!/usr/bin/env bash
# Fill ${VAR} placeholders in a template file from the environment.
set -euo pipefail

tpl="$(cat "$1")"
# Widely-copied one-liner: let the shell expand ${VAR} for us.
eval "echo \"$tpl\""
