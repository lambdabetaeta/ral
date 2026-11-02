#!/usr/bin/env bash
# Build boot.img: Ubuntu's kernel plus an initramfs carrying ral-guest as /init
# and exarch as /engine, both static musl (README.md, "boot.img"). Outputs in
# vm-image/out/boot/: kernel, initramfs.img, their .sha256, boot-manifest.txt,
# kernel-config-check.txt, verify.txt, build.log. vm_manager::media::locate and
# the bundle read those fixed paths.
#
# Usage: build-boot.sh [--clean]   (--clean drops the build cache first)
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

case "${1:-}" in
  --clean) CLEAN=1 ;;
  "") CLEAN= ;;
  *) echo "usage: build-boot.sh [--clean]" >&2; exit 2 ;;
esac

# Persistent /build: cargo's target dir and home. Nothing shipped comes from it.
# The marker confines --clean to a directory this script made.
CACHE="${RAL_BUILD_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/ral-guest-build/$ARCH}"
MARK="$CACHE/.ral-guest-build"
if [ ! -e "$MARK" ]; then
  if [ -n "$(ls -A "$CACHE" 2>/dev/null)" ]; then
    echo "error: $CACHE holds files this build did not make; point RAL_BUILD_CACHE at a new or empty directory." >&2
    exit 1
  fi
  mkdir -p "$CACHE" && touch "$MARK"
elif [ -n "$CLEAN" ]; then
  rm -rf "$CACHE" && mkdir -p "$CACHE" && touch "$MARK"
fi

# Half the cores under WSL: the other half is Windows, and a full-width release
# build exhausts the VM's memory by swapping the desktop.
if [ -n "${WSL_DISTRO_NAME:-}" ]; then
  export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$(( ($(nproc) + 1) / 2 ))}"
fi

# On drvfs every source read crosses to Windows: build from an ext4 copy
# (host target/ is another platform's; the multi-GB rootfs is not read).
SRC_DIR="$REPO_DIR"
if [ -n "${WSL_DISTRO_NAME:-}" ] && [ "${REPO_DIR#/mnt/}" != "$REPO_DIR" ]; then
  command -v rsync >/dev/null || { echo "error: rsync not found: sudo apt-get install rsync" >&2; exit 1; }
  SRC_DIR="$CACHE/src"
  echo ">> copying the checkout to $SRC_DIR (ext4)"
  rsync -a --delete --exclude '/target/' --exclude '/vm-image/out/' "$REPO_DIR/" "$SRC_DIR/"
fi

preflight
stage boot
echo ">> building boot.img: $SUITE/$ARCH kernel + initramfs, ral @ $GIT_HASH"
echo ">> reusing the build tree in $CACHE"

run_container \
  -e CARGO_BUILD_JOBS \
  -v "$SRC_DIR:/ral:ro" -v "$CACHE:/build" <<'INNER'
export DEBIAN_FRONTEND=noninteractive

echo ">> [container] installing build tooling"
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    curl ca-certificates build-essential musl-tools kmod \
    cpio gzip zstd xz-utils file binutils e2fsprogs fakeroot >/dev/null

# --- The two guests' device models, as data ----------------------------------
# CHECK_SYMS is recorded in kernel-config-check.txt; MODNAME is what ships when
# the symbol is `=m`. A symbol only in the first is declined on purpose; a
# MODNAME entry drifted from the kernel fails step 3.
case "$ARCH" in
  arm64)
    # Virtualization.framework: virtio disks, hvc0 console, virtiofs workspace,
    # virtio vsock. CONFIG_HYPERV_VSOCKETS is recorded and declined (no device).
    CHECK_SYMS=(CONFIG_VIRTIO CONFIG_VIRTIO_RING CONFIG_VIRTIO_MMIO CONFIG_VIRTIO_PCI
                CONFIG_VIRTIO_BLK CONFIG_VIRTIO_CONSOLE CONFIG_VIRTIO_FS CONFIG_VSOCKETS
                CONFIG_VIRTIO_VSOCKETS CONFIG_HYPERV_VSOCKETS CONFIG_OVERLAY_FS
                CONFIG_EXT4_FS CONFIG_DEVTMPFS CONFIG_DEVTMPFS_MOUNT CONFIG_TUN
                CONFIG_SECCOMP_FILTER CONFIG_NET_NS)
    # The virtio vsock transport is `vmw_vsock_virtio_transport` (not
    # `virtio_vsock`); modules.dep pulls `vsock` in. CONFIG_TUN backs no
    # device: the guest's only NIC is a tun to a host-side user-mode stack (§6).
    declare -A MODNAME=(
      [CONFIG_VIRTIO]=virtio [CONFIG_VIRTIO_RING]=virtio_ring
      [CONFIG_VIRTIO_MMIO]=virtio_mmio [CONFIG_VIRTIO_PCI]=virtio_pci
      [CONFIG_VIRTIO_BLK]=virtio_blk [CONFIG_VIRTIO_CONSOLE]=virtio_console
      [CONFIG_VSOCKETS]=vsock [CONFIG_VIRTIO_VSOCKETS]=vmw_vsock_virtio_transport
      [CONFIG_VIRTIO_FS]=virtiofs [CONFIG_OVERLAY_FS]=overlay
      [CONFIG_EXT4_FS]=ext4 [CONFIG_TUN]=tun
    )
    BIN_FORMAT="ARM aarch64"
    ;;
  amd64)
    # Hyper-V: `hv_vmbus` is the bus (storvsc, utils, balloon ride on it). No
    # `hv_netvsc`: the guest has no VMBus NIC (§6), so CONFIG_HYPERV_NET is only
    # recorded. vsock is `hv_sock`; CONFIG_VIRTIO_VSOCKETS is recorded and declined.
    # The workspace is a 9p share over vsock: `9pnet_fd` must be named, or on
    # kernels that split it from `9pnet` the mount fails with a bare ENODEV.
    # scsi_mod and sd_mod are `=y` today; they stay in the map so a kernel that
    # modularises them still ships them. Serial 8250 is `=y`: nothing to ship.
    # CONFIG_HYPERV (umbrella bool) is not CONFIG_HYPERV_VMBUS (the tristate):
    # the tristate carries the module name; never rely on other modules'
    # modules.dep lines to drag `hv_vmbus` in.
    CHECK_SYMS=(CONFIG_HYPERV CONFIG_HYPERV_VMBUS CONFIG_HYPERV_STORAGE
                CONFIG_HYPERV_UTILS
                CONFIG_HYPERV_BALLOON CONFIG_HYPERV_NET CONFIG_VSOCKETS
                CONFIG_HYPERV_VSOCKETS CONFIG_VIRTIO_VSOCKETS CONFIG_9P_FS
                CONFIG_NET_9P CONFIG_NET_9P_FD CONFIG_SCSI CONFIG_BLK_DEV_SD
                CONFIG_OVERLAY_FS CONFIG_EXT4_FS CONFIG_DEVTMPFS
                CONFIG_DEVTMPFS_MOUNT CONFIG_SERIAL_8250 CONFIG_TUN
                CONFIG_SECCOMP_FILTER CONFIG_NET_NS)
    # Real basenames as linux-modules ships them (`hv_storvsc`, `hv_sock`, `9p`).
    declare -A MODNAME=(
      [CONFIG_HYPERV_VMBUS]=hv_vmbus [CONFIG_HYPERV_STORAGE]=hv_storvsc
      [CONFIG_HYPERV_UTILS]=hv_utils [CONFIG_HYPERV_BALLOON]=hv_balloon
      [CONFIG_VSOCKETS]=vsock [CONFIG_HYPERV_VSOCKETS]=hv_sock
      [CONFIG_9P_FS]=9p [CONFIG_NET_9P]=9pnet [CONFIG_NET_9P_FD]=9pnet_fd
      [CONFIG_SCSI]=scsi_mod [CONFIG_BLK_DEV_SD]=sd_mod
      [CONFIG_OVERLAY_FS]=overlay [CONFIG_EXT4_FS]=ext4 [CONFIG_TUN]=tun
    )
    BIN_FORMAT="x86-64"
    ;;
esac

# musl builds report "statically linked" or "static-pie linked"; both pass.
is_guest_binary() {
  case "$(file -b "$1")" in
    *"$BIN_FORMAT"*static*) return 0 ;;
    *) return 1 ;;
  esac
}

# --- 1. Build ral-guest and exarch for the musl target ------------------------
# rustup + musl-tools, no cross-rs container. /ral is read-only, so the target
# dir and cargo home live in /build.
echo ">> [container] installing rustup ($RUST_TARGET)"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh
sh /tmp/rustup.sh -y --profile minimal --default-toolchain stable --target "$RUST_TARGET" \
    >/tmp/rustup.log 2>&1
. /root/.cargo/env
export CARGO_TARGET_DIR=/build/target
export CARGO_HOME=/build/cargo

echo ">> [container] cargo build --release --target $RUST_TARGET"
( cd /ral && cargo build --release --locked --target "$RUST_TARGET" \
    -p ral-guest -p exarch )

BIN="/build/target/$RUST_TARGET/release"
file "$BIN/ral-guest" "$BIN/exarch"
for b in ral-guest exarch; do
  is_guest_binary "$BIN/$b" \
    || { echo "error: $b is not a static $BIN_FORMAT binary: $(file -b "$BIN/$b")" >&2; exit 1; }
done

# Manifest versions come from the same checkout as the guest binaries.
echo ">> [container] building the media manifest writer"
( cd /ral && cargo build --release --locked --target "$RUST_TARGET" \
    -p ral-vm-image-build-tools --bin media-manifest )

# --- 2. Extract the pinned suite's kernel + modules --------------------------
# The base's apt points at noble; add the pinned suite. linux-image-generic
# Depends on hundreds of MB of linux-firmware this guest never loads, so a
# dry-run solve names the two real packages and `apt-get download` fetches
# only those.
echo ">> [container] resolving $SUITE/$ARCH linux-image-generic's real packages"
echo "deb [arch=$ARCH] $MIRROR $SUITE main restricted universe multiverse" \
    > /etc/apt/sources.list.d/pinned-suite.list
apt-get update -qq
SIM=$(apt-get install -s --no-install-recommends -t "$SUITE" linux-image-generic)
# `sed -n 1p`, never `head -1`: head's SIGPIPE fails the pipeline under pipefail.
IMAGE_PKG=$(echo "$SIM" | grep -oE 'linux-image-[0-9][^ ]*-generic' | sed -n 1p)
MODULES_PKG=$(echo "$SIM" | grep -oE 'linux-modules-[0-9][^ ]*-generic' | sed -n 1p)
echo ">> [container] fetching $IMAGE_PKG + $MODULES_PKG (no firmware)"
# /build persists for cargo alone; kernel and cpio staging must start empty
# (stale modules block `zstd -o`, stale .debs match the globs below).
rm -rf /build/kernel-debs /build/kernel-extract /build/cpio /build/vmlinuz
mkdir -p /build/kernel-debs /build/kernel-extract
( cd /build/kernel-debs && apt-get download "$IMAGE_PKG" "$MODULES_PKG" )
for deb in /build/kernel-debs/"$IMAGE_PKG"_*.deb /build/kernel-debs/"$MODULES_PKG"_*.deb; do
  dpkg-deb -x "$deb" /build/kernel-extract
done
# The modules ship under usr/lib/modules (usrmerge); depmod only looks in /lib/modules.
ln -sfn usr/lib /build/kernel-extract/lib

KERNEL_PATH=$(find /build/kernel-extract/boot -maxdepth 1 -name 'vmlinuz-*' | sed -n 1p)
KVER=$(basename "$KERNEL_PATH" | sed 's/^vmlinuz-//')
CONFIG_PATH="/build/kernel-extract/boot/config-$KVER"
echo ">> [container] kernel $KVER: $(file -b "$KERNEL_PATH")"

# arm64 (VZLinuxBootLoader) needs a raw ARM64 `Image`, but vmlinuz is wrapped
# (PE with a .linux section, or EFI zboot: "zimg" at byte 4, LE32 payload
# offset/size at 8/12; compressed, nested per suite): peel until it is one.
# amd64 (Hyper-V) wants a bzImage, which vmlinuz already is: copied and asserted,
# never peeled (that would yield an ELF vmlinux). The helpers serve arm64 only.
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
case "$ARCH" in
  arm64)
    unwrap_kernel /build/vmlinuz /out/kernel
    file -b /out/kernel | grep -q "ARM64 boot executable" \
      || { echo "unwrapped kernel is not a raw ARM64 Image: $(file -b /out/kernel)" >&2; exit 1; }
    echo ">> [container] unwrapped to raw Image: $(file -b /out/kernel)"
    KERNEL_NOTE="raw ARM64 Image (VZLinuxBootLoader boots nothing else)"
    ;;
  amd64)
    cp /build/vmlinuz /out/kernel
    file -b /out/kernel | grep -q "Linux kernel x86 boot executable bzImage" \
      || { echo "Ubuntu's amd64 vmlinuz is not a bzImage: $(file -b /out/kernel)" >&2; exit 1; }
    echo ">> [container] bzImage shipped as-is: $(file -b /out/kernel)"
    KERNEL_NOTE="x86 bzImage, Ubuntu's vmlinuz verbatim (Hyper-V's LinuxKernelDirect boots this)"
    ;;
esac

KERNEL_PKG_VERSION=$(dpkg-deb -f /build/kernel-debs/"$IMAGE_PKG"_*.deb Version | sed -n 1p)

# --- 3. Confirm the module set against the real kernel config ---------------
# y = compiled in; m = ship it; missing = absent from this kernel (worth seeing).
{
  echo "# boot.img kernel config check: $KVER ($ARCH)"
  for sym in "${CHECK_SYMS[@]}"; do
    grep -E "^$sym=" "$CONFIG_PATH" || echo "$sym is not set"
  done
} > /out/kernel-config-check.txt
cat /out/kernel-config-check.txt

for sym in CONFIG_SECCOMP_FILTER CONFIG_NET_NS; do
  if ! grep -qE "^$sym=y$" "$CONFIG_PATH"; then
    echo "error: $sym must be =y for the guest command jail" >&2
    exit 1
  fi
done

# Sorted: hash-table order would shuffle `modules_shipped=` between identical builds.
NEEDED_MODULES=()
for sym in $(printf '%s\n' "${!MODNAME[@]}" | sort); do
  grep -qE "^$sym=m$" "$CONFIG_PATH" && NEEDED_MODULES+=("${MODNAME[$sym]}")
done

mkdir -p /build/cpio/modules
: > /build/cpio/modules.order
if [ "${#NEEDED_MODULES[@]}" -gt 0 ]; then
  echo ">> [container] modules to ship: ${NEEDED_MODULES[*]}"
  depmod -b /build/kernel-extract "$KVER"
  MODDEP="/build/kernel-extract/lib/modules/$KVER/modules.dep"

  # The requested modules plus their dependencies, each with its own dependency
  # line: the ordering below needs the graph, not just the roots.
  declare -A DEPS_OF=()
  CLOSURE=()
  for name in "${NEEDED_MODULES[@]}"; do
    line=$(grep -m1 "^[^:]*/${name}\.ko[^:]*:" "$MODDEP" || true)
    if [ -z "$line" ]; then
      # MODNAME has drifted from the kernel; the guest would fail at mount.
      echo "error: $name is =m in the kernel config but not in modules.dep" >&2
      exit 1
    fi
    # A leaf's line ends at the colon ("vsock.ko.zst:"), so ${line#*:} is empty.
    for modpath in "${line%%:*}" ${line#*:}; do
      [ -n "${DEPS_OF[$modpath]+recorded}" ] && continue
      own=$(grep -m1 "^${modpath}:" "$MODDEP")
      DEPS_OF[$modpath]="${own#*:}"
      CLOSURE+=("$modpath")
    done
  done

  # Load order is computed: modules.dep lists dependencies as an unordered set
  # (`9pnet_fd: 9pnet netfs`, yet 9pnet needs netfs first). A pass that emits
  # nothing means a dependency cycle.
  ORDER=/tmp/module-order
  : > "$ORDER"
  emitted=" "
  remaining=($(printf '%s\n' "${CLOSURE[@]}" | sort))
  while [ "${#remaining[@]}" -gt 0 ]; do
    stuck=()
    for modpath in "${remaining[@]}"; do
      ready=yes
      for dep in ${DEPS_OF[$modpath]}; do
        case "$emitted" in *" $dep "*) ;; *) ready=no ;; esac
      done
      if [ "$ready" = yes ]; then
        echo "$modpath" >> "$ORDER"
        emitted="$emitted$modpath "
      else
        stuck+=("$modpath")
      fi
    done
    if [ "${#stuck[@]}" -eq "${#remaining[@]}" ]; then
      echo "error: circular kernel module dependencies among: ${stuck[*]}" >&2
      exit 1
    fi
    remaining=("${stuck[@]}")
  done

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
  done < "$ORDER"
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
cp "$BIN/ral-guest" /build/cpio/init
cp "$BIN/exarch" /build/cpio/engine
install -d /build/cpio/usr/share/doc/exarch
install -m 0644 /ral/LICENSE-APACHE /ral/LICENSE-MIT /ral/NOTICE /build/cpio/usr/share/doc/exarch/
install -m 0644 /ral/vendor/edtui/LICENSE.md /build/cpio/usr/share/doc/exarch/LICENSE-EDTUI
chmod 0755 /build/cpio/init /build/cpio/engine /build/cpio/sbin/mke2fs
# /dev/console is init's stdio; rootless podman has no CAP_MKNOD, so the node
# and the archive recording it are made under fakeroot.
echo ">> [container] assembling initramfs.img"
fakeroot sh -c '
  mkdir -p /build/cpio/dev
  mknod -m 600 /build/cpio/dev/console c 5 1
  cd /build/cpio && find . | cpio -o -H newc 2>/dev/null | gzip -9
' > /out/initramfs.img

( cd /out && sha256sum kernel > kernel.sha256 && sha256sum initramfs.img > initramfs.img.sha256 )

# --- 6. Verify without booting ------------------------------------------------
{
  echo "# boot.img verification: $SUITE/$ARCH, kernel $KVER"
  echo
  echo "== kernel: $KERNEL_NOTE =="
  file /out/kernel
  echo
  echo "== ral-guest / exarch: static, $BIN_FORMAT =="
  file "$BIN/ral-guest" "$BIN/exarch"
  for b in ral-guest exarch; do
    is_guest_binary "$BIN/$b" && echo "OK   $b" || echo "WRONG $b"
  done
  echo
  echo "== modules shipped, in the load order /modules.order fixes =="
  cat /build/cpio/modules.order
  echo
  echo "== mke2fs: every ldd dependency vendored =="
  for lib in $LIBS; do
    [ -f "/build/cpio$lib" ] && echo "OK   $lib" || echo "MISSING $lib"
  done
  echo
  echo "== cpio path manifest =="
  for path in init engine sbin/mke2fs modules.order dev/console \
    usr/share/doc/exarch/LICENSE-APACHE usr/share/doc/exarch/LICENSE-MIT \
    usr/share/doc/exarch/NOTICE usr/share/doc/exarch/LICENSE-EDTUI; do
    [ -e "/build/cpio/$path" ] && echo "OK   /$path" || echo "MISSING /$path"
  done
  [ -d /build/cpio/modules ] && echo "OK   /modules (${#NEEDED_MODULES[@]} driver(s) requested)"
  echo
  # Native container (preflight), so the guest's own binaries really run.
  echo "== version probes (native $ARCH container) =="
  /build/cpio/sbin/mke2fs -V 2>&1 | sed -n 1p
  "$BIN/ral-guest" --help >/dev/null 2>&1 || echo "ral-guest: refuses off pid 1, as designed"
  # `--help`: exarch has no version flag.
  "$BIN/exarch" --help 2>&1 | sed -n 1p || true
} > /out/verify.txt
cat /out/verify.txt

# `arch=` leads: out/boot/ encodes the guest in no path.
{
  echo "arch=$ARCH"
  echo "kernel_format=$KERNEL_NOTE"
  echo "kernel_package=$KERNEL_PKG_VERSION"
  echo "kernel_version=$KVER"
  echo "suite=$SUITE arch=$ARCH mirror=$MIRROR"
  echo "ral_git_hash=$GIT_HASH"
  "$BIN/media-manifest"
  echo "rust_target=$RUST_TARGET"
  echo "modules_shipped=${NEEDED_MODULES[*]:-none}"
} > /out/boot-manifest.txt
cat /out/boot-manifest.txt

echo ">> [container] done"
INNER

publish boot-manifest.txt

echo ">> kernel:     $OUT_DIR/kernel"
echo ">> initramfs:  $OUT_DIR/initramfs.img"
echo ">> manifest + verification in $OUT_DIR/"
