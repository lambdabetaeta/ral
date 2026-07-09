#!/usr/bin/env bash
# Evaluate a simple integer arithmetic expression given as arguments.
set -euo pipefail
echo "$(( $* ))"
