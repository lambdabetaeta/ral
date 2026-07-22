#!/usr/bin/env bash
#
# Build boot.img: the arm64 kernel + initramfs synod ships (SYNOD.md §7's
# third disk). Unlike rootfs.img, boot.img carries the ral binaries — the
# kernel is Ubuntu's own linux-generic (already built, signed, and validated
# by Canonical across the exact virtio + overlayfs + ext4 feature set this
# guest needs); the initramfs is this repo's own /init (ral-initramfs) plus
# ral-daemon and exarch, statically built for aarch64-unknown-linux-musl.
#
# rootfs.img carries no ral binaries (vm-image/README.md). So the initramfs
# does the one thing that makes boot.img's "kernel + ral-daemon + ral engine
# as one artifact" true: on every boot it mounts rootfs.img read-only and a
# freshly mke2fs'd session.img read-write as an overlay, COPIES ral-daemon
# and exarch onto that writable upper at /sbin/ral-daemon and
# /usr/libexec/ral/engine, then switch_roots and execs /sbin/ral-daemon —
# neither binary is baked into the read-only rootfs.
#
# The whole build runs inside one native-arm64 ubuntu:24.04 container, the
# same base build.sh uses: rustup + musl-tools cross-builds the three Rust
# binaries (build-binaries.yml's build-exarch-linux-arm64 recipe, no cross-rs
# container needed — neither binary pulls jemalloc-sys or a C dependency);
# apt fetches Ubuntu's real linux-image-generic package for the pinned suite
# to extract vmlinuz and its modules; e2fsprogs's mke2fs is vendored together
# with its ldd-resolved shared libraries, all resolved in the identical
# container so glibc versions match exactly.
#
# Outputs (in vm-image/out/boot/, on the Mac via a bind mount):
#   kernel                  raw ARM64 Image, unwrapped from Ubuntu's vmlinuz
#   initramfs.img           cpio.gz: /init, ral-daemon, exarch, kernel
#                           modules, vendored mke2fs
#   kernel.sha256, initramfs.img.sha256
#   boot-manifest.txt       kernel package version + built git commit hash
#   kernel-config-check.txt real CONFIG_* values behind the module set
#   verify.txt              static-link / arch / cpio manifest spot-checks
#   build.log               full build transcript

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT_DIR="$SCRIPT_DIR/out/boot"

# --- Knobs (env-overridable) -------------------------------------------------
SUITE="${SUITE:-resolute}"                 # matches rootfs.img's pinned suite
ARCH="${ARCH:-arm64}"
MIRROR="${MIRROR:-http://ports.ubuntu.com/ubuntu-ports}"
BASE_IMAGE="${BASE_IMAGE:-docker.io/library/ubuntu:24.04}"
RUST_TARGET="${RUST_TARGET:-aarch64-unknown-linux-musl}"

mkdir -p "$OUT_DIR"
export SUITE ARCH MIRROR RUST_TARGET

command -v podman >/dev/null || { echo "podman not found" >&2; exit 1; }

# The maintainer benchmarks exarch per commit (dev/docs' own convention), so
# the source hash built into this artifact is recorded from the tree itself,
# not from any binary's own --version.
GIT_HASH="$(git -C "$REPO_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
export GIT_HASH

echo ">> building boot.img: $SUITE/$ARCH kernel + initramfs, ral @ $GIT_HASH"
START=$(date +%s)

podman run --rm -i \
  --privileged \
  --security-opt label=disable \
  -e SUITE -e ARCH -e MIRROR -e RUST_TARGET -e GIT_HASH \
  -v "$REPO_DIR:/ral:ro" \
  -v "$OUT_DIR:/out" \
  "$BASE_IMAGE" \
  bash -euo pipefail -s <<'INNER' 2>&1 | tee "$OUT_DIR/build.log"
export DEBIAN_FRONTEND=noninteractive

echo ">> [container] installing build tooling"
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    curl ca-certificates build-essential musl-tools kmod \
    cpio gzip zstd xz-utils file binutils e2fsprogs >/dev/null

# --- 1. Cross-build ral-daemon, exarch, ral-initramfs -----------------------
# Same recipe as build-binaries.yml's build-exarch-linux-arm64 job: rustup +
# musl-tools, no cross-rs container. /ral is read-only, so the target dir and
# cargo home move off it.
echo ">> [container] installing rustup ($RUST_TARGET)"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh
sh /tmp/rustup.sh -y --profile minimal --default-toolchain stable --target "$RUST_TARGET" \
    >/tmp/rustup.log 2>&1
. /root/.cargo/env
export CARGO_TARGET_DIR=/build/target

echo ">> [container] cargo build --release --target $RUST_TARGET"
( cd /ral && cargo build --release --locked --target "$RUST_TARGET" \
    -p ral-daemon -p exarch -p ral-initramfs )

BIN="/build/target/$RUST_TARGET/release"
file "$BIN/ral-daemon" "$BIN/exarch" "$BIN/ral-initramfs"

# --- 2. Extract the pinned suite's kernel + modules --------------------------
# ubuntu:24.04's own apt sources point at noble; add the pinned suite as an
# extra source (same signing keyring, per rootfs.img's build.sh). The
# `linux-image-generic` meta-package's own Depends pulls linux-firmware's
# many hardware-specific blobs (100s of MB) that this guest, with no real
# hardware behind it, will never load — so the two real per-version packages
# it resolves to are named via a dry-run solve, then `apt-get download`,
# which fetches exactly the packages named and nothing their Depends ask
# for, targets only those two.
echo ">> [container] resolving $SUITE/$ARCH linux-image-generic's real packages"
echo "deb [arch=$ARCH] $MIRROR $SUITE main restricted universe multiverse" \
    > /etc/apt/sources.list.d/pinned-suite.list
apt-get update -qq
SIM=$(apt-get install -s --no-install-recommends -t "$SUITE" linux-image-generic)
IMAGE_PKG=$(echo "$SIM" | grep -oE 'linux-image-[0-9][^ ]*-generic' | head -1)
MODULES_PKG=$(echo "$SIM" | grep -oE 'linux-modules-[0-9][^ ]*-generic' | head -1)
echo ">> [container] fetching $IMAGE_PKG + $MODULES_PKG (no firmware)"
mkdir -p /build/kernel-debs /build/kernel-extract
( cd /build/kernel-debs && apt-get download "$IMAGE_PKG" "$MODULES_PKG" )
for deb in /build/kernel-debs/"$IMAGE_PKG"_*.deb /build/kernel-debs/"$MODULES_PKG"_*.deb; do
  dpkg-deb -x "$deb" /build/kernel-extract
done
# dpkg-deb -x unpacks the package's own literal paths, which the modules
# package ships as usr/lib/modules/... (Ubuntu's usrmerge layout); a real
# root has /lib -> /usr/lib to make that reachable at /lib/modules too,
# which is the one path depmod itself will ever look under.
ln -sfn usr/lib /build/kernel-extract/lib

KERNEL_PATH=$(find /build/kernel-extract/boot -maxdepth 1 -name 'vmlinuz-*' | head -1)
KVER=$(basename "$KERNEL_PATH" | sed 's/^vmlinuz-//')
CONFIG_PATH="/build/kernel-extract/boot/config-$KVER"
echo ">> [container] kernel $KVER: $(file -b "$KERNEL_PATH")"

# VZLinuxBootLoader on Apple silicon boots only an uncompressed ARM64 Image;
# Ubuntu ships vmlinuz wrapped — a unified-image PE whose kernel is its
# .linux section, an EFI zboot PE ("zimg" magic at byte 4, LE32 payload
# offset/size at bytes 8/12), and a compressed payload, nested per suite —
# so peel whichever layers this vmlinuz has and refuse anything unknown.
le16() { od -An -tu2 -j"$2" -N2 "$1" | tr -d ' '; }
le32() { od -An -tu4 -j"$2" -N4 "$1" | tr -d ' '; }

# Extract a named PE section to stdout; empty output if absent.
pe_section() {
  local pe nsec optsz secs i off name rawsz rawoff
  pe=$(le32 "$1" 60)
  nsec=$(le16 "$1" $((pe + 6)))
  optsz=$(le16 "$1" $((pe + 20)))
  secs=$((pe + 24 + optsz))
  for ((i = 0; i < nsec; i++)); do
    off=$((secs + 40 * i))
    name=$(dd if="$1" bs=1 skip="$off" count=8 2>/dev/null | tr -d '\0')
    [ "$name" = "$2" ] || continue
    rawsz=$(le32 "$1" $((off + 16)))
    rawoff=$(le32 "$1" $((off + 20)))
    tail -c +"$((rawoff + 1))" "$1" | head -c "$rawsz"
    return 0
  done
}

unwrap_kernel() {
  case "$(file -b "$1")" in
    *"ARM64 boot executable"*) cp "$1" "$2" ;;
    *"gzip compressed"*)       zcat < "$1" > "$2" ;;
    *"Zstandard compressed"*)  zstd -dqc "$1" > "$2" ;;
    *PE32+*)
      if [ "$(dd if="$1" bs=1 skip=4 count=4 2>/dev/null | tr -d '\0')" = zimg ]; then
        local off size
        off=$(le32 "$1" 8)
        size=$(le32 "$1" 12)
        tail -c +"$((off + 1))" "$1" | head -c "$size" > "$1.payload"
        unwrap_kernel "$1.payload" "$2"
      else
        pe_section "$1" .linux > "$1.linux"
        [ -s "$1.linux" ] \
          || { echo "PE kernel with neither zboot magic nor a .linux section: $1" >&2; exit 1; }
        unwrap_kernel "$1.linux" "$2"
      fi ;;
    *) echo "unrecognised kernel format: $(file -b "$1")" >&2; exit 1 ;;
  esac
}
cp "$KERNEL_PATH" /build/vmlinuz
unwrap_kernel /build/vmlinuz /out/kernel
file -b /out/kernel | grep -q "ARM64 boot executable" \
  || { echo "unwrapped kernel is not a raw ARM64 Image: $(file -b /out/kernel)" >&2; exit 1; }
echo ">> [container] unwrapped to raw Image: $(file -b /out/kernel)"

KERNEL_PKG_VERSION=$(dpkg-deb -f /build/kernel-debs/"$IMAGE_PKG"_*.deb Version | head -1)

# --- 3. Confirm the module set against the real kernel config ---------------
# README.md's prose named a baseline; this is the pass that checks it against
# what the pinned kernel package actually built. y = compiled in (nothing to
# ship); m = a module this initramfs must carry and load; both virtio
# transports are shipped if both are modules, since which one
# Virtualization.framework attaches on is exactly what the smoke-boot (not
# this non-booting pass) confirms.
{
  echo "# boot.img kernel config check — $KVER"
  for sym in CONFIG_VIRTIO CONFIG_VIRTIO_RING CONFIG_VIRTIO_MMIO CONFIG_VIRTIO_PCI \
             CONFIG_VIRTIO_BLK CONFIG_VIRTIO_CONSOLE CONFIG_VSOCKETS \
             CONFIG_VIRTIO_VSOCKETS CONFIG_HYPERV_VSOCKETS CONFIG_OVERLAY_FS \
             CONFIG_EXT4_FS CONFIG_DEVTMPFS CONFIG_DEVTMPFS_MOUNT; do
    grep -E "^$sym=" "$CONFIG_PATH" || echo "$sym is not set"
  done
} > /out/kernel-config-check.txt
cat /out/kernel-config-check.txt

# Logical driver name -> real module basename, for whichever of the above
# are `m`. The vsock family's real names (confirmed against this kernel's
# own modules.dep, not guessed from its Kconfig symbol): the virtio
# transport is `vmw_vsock_virtio_transport`, not `virtio_vsock`; the
# Hyper-V transport is `hv_sock`; both pull the core `vsock` module in
# automatically via their own modules.dep line.
declare -A MODNAME=(
  [CONFIG_VIRTIO]=virtio [CONFIG_VIRTIO_RING]=virtio_ring
  [CONFIG_VIRTIO_MMIO]=virtio_mmio [CONFIG_VIRTIO_PCI]=virtio_pci
  [CONFIG_VIRTIO_BLK]=virtio_blk [CONFIG_VIRTIO_CONSOLE]=virtio_console
  [CONFIG_VSOCKETS]=vsock [CONFIG_VIRTIO_VSOCKETS]=vmw_vsock_virtio_transport
  [CONFIG_HYPERV_VSOCKETS]=hv_sock [CONFIG_OVERLAY_FS]=overlay
  [CONFIG_EXT4_FS]=ext4
)
NEEDED_MODULES=()
for sym in "${!MODNAME[@]}"; do
  grep -qE "^$sym=m$" "$CONFIG_PATH" && NEEDED_MODULES+=("${MODNAME[$sym]}")
done

mkdir -p /build/cpio/modules
: > /build/cpio/modules.order
if [ "${#NEEDED_MODULES[@]}" -gt 0 ]; then
  echo ">> [container] modules to ship: ${NEEDED_MODULES[*]}"
  depmod -b /build/kernel-extract "$KVER"
  MODDEP="/build/kernel-extract/lib/modules/$KVER/modules.dep"
  ORDER=/tmp/module-order.raw
  : > "$ORDER"
  for name in "${NEEDED_MODULES[@]}"; do
    line=$(grep -m1 "/${name}\.ko[^:]*:" "$MODDEP" || true)
    if [ -z "$line" ]; then
      # =m in the config but absent from modules.dep: the MODNAME table has
      # drifted from the kernel, and the boot artifact would fail at mount.
      echo "error: $name is =m in the kernel config but not in modules.dep" >&2
      exit 1
    fi
    target="${line%%:*}"
    # A leaf module's line ends right at the colon with no trailing space
    # (e.g. "vsock.ko.zst:"); requiring "*: " here would leave `deps` as the
    # whole "target:" string instead of empty. Word-splitting below already
    # ignores the one leading space a non-empty deps list leaves behind.
    deps="${line#*:}"
    for dep in $deps "$target"; do
      echo "$dep" >> "$ORDER"
    done
  done
  # First occurrence wins: each module's own modules.dep line already lists
  # its full transitive dependency chain in a safe load order, so
  # de-duplicating a concatenation of those lines preserves it.
  awk '!seen[$0]++' "$ORDER" > /tmp/module-order.dedup
  while read -r modpath; do
    [ -n "$modpath" ] || continue
    src="/build/kernel-extract/lib/modules/$KVER/$modpath"
    base=$(basename "$modpath")
    case "$base" in
      *.ko.zst) zstd -dq "$src" -o "/build/cpio/modules/${base%.zst}"; base="${base%.zst}" ;;
      *.ko.xz)  xz -dkc "$src" > "/build/cpio/modules/${base%.xz}"; base="${base%.xz}" ;;
      *.ko)     cp "$src" "/build/cpio/modules/$base" ;;
      *) echo "unrecognised module compression: $modpath" >&2; exit 1 ;;
    esac
    echo "$base" >> /build/cpio/modules.order
  done < /tmp/module-order.dedup
fi

# --- 4. Vendor mke2fs + its resolved shared libraries ------------------------
MKE2FS_BIN=$(readlink -f "$(command -v mke2fs)")
mkdir -p /build/cpio/sbin
cp "$MKE2FS_BIN" /build/cpio/sbin/mke2fs
LIBS=$(ldd "$MKE2FS_BIN" | awk '{for (i=1;i<=NF;i++) if ($i ~ /^\//) print $i}' | sort -u)
for lib in $LIBS; do
  mkdir -p "/build/cpio$(dirname "$lib")"
  cp -L "$lib" "/build/cpio$lib"
done
echo ">> [container] mke2fs vendored with $(echo "$LIBS" | wc -l) shared libraries"

# --- 5. Assemble the cpio tree -----------------------------------------------
cp "$BIN/ral-initramfs" /build/cpio/init
cp "$BIN/ral-daemon" /build/cpio/ral-daemon
cp "$BIN/exarch" /build/cpio/engine
chmod 0755 /build/cpio/init /build/cpio/ral-daemon /build/cpio/engine /build/cpio/sbin/mke2fs
# The node the kernel opens as init's stdio; /init mounts devtmpfs over
# /dev itself before it needs any device beyond the console.
mkdir -p /build/cpio/dev
mknod -m 600 /build/cpio/dev/console c 5 1

echo ">> [container] assembling initramfs.img"
( cd /build/cpio && find . | cpio -o -H newc 2>/dev/null | gzip -9 ) > /out/initramfs.img

( cd /out && sha256sum kernel > kernel.sha256 && sha256sum initramfs.img > initramfs.img.sha256 )

# --- 6. Verify without booting ------------------------------------------------
{
  echo "# boot.img verification — $SUITE/$ARCH, kernel $KVER"
  echo
  echo "== kernel: raw ARM64 Image (VZLinuxBootLoader boots nothing else) =="
  file /out/kernel
  echo
  echo "== ral-daemon / exarch / ral-initramfs: static, aarch64 =="
  file "$BIN/ral-daemon" "$BIN/exarch" "$BIN/ral-initramfs"
  echo
  echo "== mke2fs: every ldd dependency vendored =="
  for lib in $LIBS; do
    [ -f "/build/cpio$lib" ] && echo "OK   $lib" || echo "MISSING $lib"
  done
  echo
  echo "== cpio path manifest =="
  for path in init ral-daemon engine sbin/mke2fs modules.order dev/console; do
    [ -e "/build/cpio/$path" ] && echo "OK   /$path" || echo "MISSING /$path"
  done
  [ -d /build/cpio/modules ] && echo "OK   /modules (${#NEEDED_MODULES[@]} driver(s) requested)"
  echo
  echo "== version probes (native aarch64 container, no qemu) =="
  /build/cpio/sbin/mke2fs -V 2>&1 | head -1
  "$BIN/ral-daemon" --help >/dev/null 2>&1 || echo "ral-daemon: refuses off pid 1, as designed"
  "$BIN/exarch" --version 2>&1 | head -1 || true
} > /out/verify.txt
cat /out/verify.txt

{
  echo "kernel_package=$KERNEL_PKG_VERSION"
  echo "kernel_version=$KVER"
  echo "suite=$SUITE arch=$ARCH mirror=$MIRROR"
  echo "ral_git_hash=$GIT_HASH"
  echo "rust_target=$RUST_TARGET"
  echo "modules_shipped=${NEEDED_MODULES[*]:-none}"
} > /out/boot-manifest.txt
cat /out/boot-manifest.txt

echo ">> [container] done"
INNER

END=$(date +%s)
echo
echo ">> BUILD COMPLETE in $(( END - START ))s"
echo ">> kernel:     $OUT_DIR/kernel"
echo ">> initramfs:  $OUT_DIR/initramfs.img"
echo ">> manifest + verification in $OUT_DIR/"
