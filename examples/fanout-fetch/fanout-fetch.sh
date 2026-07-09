#!/usr/bin/env bash
# Fetch a list of URLs in parallel into a directory.
set -euo pipefail

list_file="$1"; out_dir="$2"

mkdir -p "$out_dir"

xargs -P 8 -I{} curl -fsSL --max-time 15 -o "$out_dir/$(basename {})" {} < "$list_file"
