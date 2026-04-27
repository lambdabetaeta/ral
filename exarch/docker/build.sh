#!/bin/sh
# Build the exarch binary inside `shell-dev` (so it's a Linux ELF) and
# stage it next to this Dockerfile, then build the image.

set -eu

cd "$(dirname "$0")/../.."

if ! docker ps --format '{{.Names}}' | grep -qx shell-dev; then
    echo "build.sh: shell-dev container not running — start it with"
    echo "          docker compose -f dev/compose.yaml up -d --build"
    exit 1
fi

docker exec shell-dev cargo build --release --locked -p exarch
cp target/release/exarch exarch/docker/exarch-linux

docker compose -f exarch/docker/compose.local.yaml build
