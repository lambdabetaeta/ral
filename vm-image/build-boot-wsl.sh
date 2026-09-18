#!/usr/bin/env bash
# Build the guest boot media from WSL, on the Linux VM's own ext4 disk.
#
# `build-boot.sh` bind-mounts the checkout it is run from. On a Windows host
# that checkout lives on C:, so every source read the container makes crosses
# drvfs — and `cargo build` makes a great many. This copies the tree onto ext4
# first, builds there, and copies the finished media back. Same script, same
# container, one filesystem.
#
# It also keeps the container's build tree between runs: `build-boot.sh` puts
# cargo's target dir and cargo home under /build, which is thrown away with the
# container, so every build is a cold one. `RAL_BUILD_CACHE` (set here by
# default) mounts a directory there instead. Nothing shipped comes out of it —
# the media is built from the copied checkout every time — so a stale cache
# costs nothing but the disk it sits on, and `--clean` drops it.
#
# Usage, from inside WSL:  ARCH=amd64 bash vm-image/build-boot-wsl.sh [--clean]
# From Windows:            just guest-boot-wsl
#
# Nothing on the Windows side may hold the container's stdout: kill whatever
# ran `wsl.exe` and the pipe goes with it, leaving orphaned rustc processes
# compiling into a closed descriptor. To run it unattended, detach it inside
# the VM — `setsid nohup bash vm-image/build-boot-wsl.sh > ~/guest-boot.log
# 2>&1 &` — and read the log.
set -euo pipefail

ARCH="${ARCH:-amd64}"
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${RAL_WSL_WORK:-$HOME/.cache/ral-guest-build}"
# Half the cores, because the other half of this machine is Windows. A release
# build of exarch's tree at full width holds more rustc processes than the WSL
# VM has memory for, and what the VM takes it takes from the desktop: the
# symptom is the host swapping, not the build failing. `JOBS=` overrides.
export CARGO_BUILD_JOBS="${JOBS:-$(( ($(nproc) + 1) / 2 ))}"

case "$SRC" in
  /mnt/*) ;;
  *) echo "this checkout is already on a Linux filesystem: run vm-image/build-boot.sh" >&2
     exit 1 ;;
esac
command -v rsync >/dev/null || { echo "rsync not found: sudo apt-get install rsync" >&2; exit 1; }

[ "${1:-}" = "--clean" ] && rm -rf "$WORK/cache"
mkdir -p "$WORK/cache"

# --exclude target/: the host's Windows build tree is another platform's, and
# the container has its own under $WORK/cache. --exclude vm-image/out/: the
# rootfs alone is 2.5G, and the media is what this build writes, not reads.
echo ">> copying the checkout to $WORK/ral (ext4)"
rsync -a --delete \
  --exclude '/target/' --exclude '/vm-image/out/' \
  "$SRC/" "$WORK/ral/"

RAL_BUILD_CACHE="$WORK/cache" ARCH="$ARCH" bash "$WORK/ral/vm-image/build-boot.sh"

echo ">> copying the media back to $SRC/vm-image/out/boot"
mkdir -p "$SRC/vm-image/out/boot"
cp -a "$WORK/ral/vm-image/out/boot/." "$SRC/vm-image/out/boot/"
