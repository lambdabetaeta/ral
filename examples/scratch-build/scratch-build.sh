#!/usr/bin/env bash
# Archive a tree via two scratch directories, removing both on any exit.
set -euo pipefail

src="$1"
out="$2"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

pack_into() {
    local pack
    pack=$(mktemp -d)
    trap 'rm -rf "$pack"' EXIT
    tar -C "$stage" -czf "$pack/tree.tar.gz" tree
    mv "$pack/tree.tar.gz" "$out"
}

cp -R "$src" "$stage/tree"
pack_into

echo "wrote $out"
