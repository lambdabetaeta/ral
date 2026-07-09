#!/usr/bin/env bash
# Create a directory/file skeleton from a manifest of relative paths.
set -euo pipefail
root=$1; manifest=$2
for rel in $(cat "$manifest"); do
    case "$rel" in
        */) mkdir -p "$root/$rel" ;;
        *)  mkdir -p "$(dirname "$root/$rel")"; touch "$root/$rel" ;;
    esac
done
