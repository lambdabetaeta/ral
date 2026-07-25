#!/usr/bin/env bash
#
# Build boot.img: the kernel + initramfs synod ships (SYNOD.md §7's third
# disk), for either of §2's two guests — arm64 under Virtualization.framework
# on macOS, or x86_64 under Hyper-V on Windows. `ARCH` selects which, arm64
# by default, and everything else about the architecture follows from it: the
# mirror, the Rust target, the boot format the hypervisor's loader accepts,
# and which drivers the guest's device model needs. There is one script
# because there is one guest: the same /init, the same daemon, the same
# engine, the same overlay — only the hardware underneath differs.
#
# Unlike rootfs.img, boot.img carries the ral binaries — the kernel is
# Ubuntu's own linux-generic (already built, signed, and validated by
# Canonical across the exact virtio + VMBus + overlayfs + ext4 feature set
# these guests need); the initramfs is this repo's own /init (ral-initramfs)
# plus ral-daemon and exarch, statically built for the guest's musl target.
#
# rootfs.img carries no ral binaries (vm-image/README.md). So the initramfs
# does the one thing that makes boot.img's "kernel + ral-daemon + ral engine
# as one artifact" true: on every boot it mounts rootfs.img read-only and a
# freshly mke2fs'd session.img read-write as an overlay, COPIES ral-daemon
# and exarch onto that writable upper at /sbin/ral-daemon and
# /usr/libexec/ral/engine, then switch_roots and execs /sbin/ral-daemon —
# neither binary is baked into the read-only rootfs.
#
# The whole build runs inside one ubuntu:24.04 container of the guest's OWN
# architecture, the same base build.sh uses. That image is multi-arch, so
# podman pulls the host's native build: the arm64 media is built on Apple
# silicon, the amd64 media on an x86_64 host (a Linux box, or WSL2 on the
# Windows machine that will run it). Nothing here is cross-architecture —
# apt resolves the kernel package for the container's own architecture and
# verify.txt executes the binaries it checks — so the first check inside
# refuses a container that is not $ARCH rather than let qemu-user emulation, or
# a confusing apt failure, appear halfway through. Inside: rustup + musl-tools
# cross-builds the three Rust binaries (build-binaries.yml's musl recipe, e.g.
# its build-exarch-linux-arm64 job — no cross-rs container needed, since neither
# binary pulls jemalloc-sys or a C dependency); apt fetches Ubuntu's real
# linux-image-generic package for the pinned suite to extract vmlinuz and its
# modules; e2fsprogs's mke2fs is vendored together with its ldd-resolved
# shared libraries, all resolved in the identical container so glibc versions
# match exactly.
#
# Outputs (in vm-image/out/boot/, on the host via a bind mount):
#   kernel                  arm64: raw ARM64 Image, unwrapped from Ubuntu's
#                           vmlinuz. amd64: that same vmlinuz verbatim, which
#                           already IS an x86 bzImage
#   initramfs.img           cpio.gz: /init, ral-daemon, exarch, kernel
#                           modules, vendored mke2fs
#   kernel.sha256, initramfs.img.sha256
#   boot-manifest.txt       arch, kernel package version, built git commit
#   kernel-config-check.txt real CONFIG_* values behind the module set
#   verify.txt              static-link / arch / cpio manifest spot-checks
#   build.log               full build transcript
#
# The output paths deliberately do not encode the architecture: a developer
# builds the media for their own host, one arch at a time, and synod's
# boot-media discovery (synod/src/boot.rs) looks in this one place — as does
# the bundle's resource map (synod/tauri.conf.json and its Windows
# counterpart). boot-manifest.txt's `arch=` line is how you know which guest
# is currently sitting in out/boot/.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT_DIR="$SCRIPT_DIR/out/boot"

# --- Knobs (env-overridable) -------------------------------------------------
SUITE="${SUITE:-resolute}"                 # matches rootfs.img's pinned suite
ARCH="${ARCH:-arm64}"                      # arm64 (macOS/VZ) | amd64 (Windows/Hyper-V)
BASE_IMAGE="${BASE_IMAGE:-docker.io/library/ubuntu:24.04}"

# Two facts follow from the architecture alone, and getting either wrong is an
# hour lost to a 404: arm64 packages are served from ports.ubuntu.com and
# amd64 packages from archive.ubuntu.com — never the other way round — and
# each guest wants its own musl target. Both are derived here rather than
# duplicated at the call site, and both still yield to an explicit override:
# `${VAR:-derived}` keeps a caller's MIRROR (a snapshot.ubuntu.com timestamp,
# say, per README's reproducibility note) or RUST_TARGET winning.
case "$ARCH" in
  arm64) ARCH_MIRROR="http://ports.ubuntu.com/ubuntu-ports"
         ARCH_RUST_TARGET="aarch64-unknown-linux-musl" ;;
  amd64) ARCH_MIRROR="http://archive.ubuntu.com/ubuntu"
         ARCH_RUST_TARGET="x86_64-unknown-linux-musl" ;;
  *) echo "ARCH=$ARCH is neither of synod's two guests (SYNOD.md §2)." >&2
     echo "Did you mean ARCH=arm64 (macOS, Virtualization.framework) or" >&2
     echo "ARCH=amd64 (Windows, Hyper-V)? Debian architecture names, not" >&2
     echo "uname's: 'amd64', not 'x86_64'." >&2
     exit 1 ;;
esac
MIRROR="${MIRROR:-$ARCH_MIRROR}"
RUST_TARGET="${RUST_TARGET:-$ARCH_RUST_TARGET}"

mkdir -p "$OUT_DIR"
export SUITE ARCH MIRROR RUST_TARGET

command -v podman >/dev/null || { echo "podman not found" >&2; exit 1; }

# Podman on Windows is a Windows program driven from a POSIX shell, and both
# halves of that need disarming before a bind mount survives the trip.  The MSYS
# runtime rewrites any argument that looks like a Unix path when it hands argv
# to a native binary, which turns the *container* side of `-v host:/ral:ro` into
# a Windows path and gets the mount rejected outright; and podman itself wants
# the *host* side spelled the way Windows spells it.  Both are handled here
# rather than in whoever invokes this, so one recipe works from PowerShell, Git
# Bash, or WSL instead of three sets of instructions.  On a real Unix host
# `cygpath` does not exist, the two variables mean nothing, and `host_path` is
# the identity.
if command -v cygpath >/dev/null 2>&1; then
  export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'
  host_path() { cygpath -w "$1"; }
else
  host_path() { printf '%s' "$1"; }
fi

# The maintainer benchmarks exarch per commit (dev/docs' own convention), so
# the source hash built into this artifact is recorded from the tree itself,
# not from any binary's own --version.
#
# `host_path` because the disarming just above applies to every native binary
# this script runs, not only podman: git on Windows is one too, and handed a
# `/c/...` argument with MSYS conversion off it reports the repository missing
# and the hash silently becomes `unknown`.
GIT_HASH="$(git -C "$(host_path "$REPO_DIR")" rev-parse --short HEAD 2>/dev/null || echo unknown)"
export GIT_HASH

echo ">> building boot.img: $SUITE/$ARCH kernel + initramfs, ral @ $GIT_HASH"
START=$(date +%s)

podman run --rm -i \
  --privileged \
  --security-opt label=disable \
  -e SUITE -e ARCH -e MIRROR -e RUST_TARGET -e GIT_HASH \
  -v "$(host_path "$REPO_DIR"):/ral:ro" \
  -v "$(host_path "$OUT_DIR"):/out" \
  "$BASE_IMAGE" \
  bash -euo pipefail -s <<'INNER' 2>&1 | tee "$OUT_DIR/build.log"
export DEBIAN_FRONTEND=noninteractive

echo ">> [container] installing build tooling"
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    curl ca-certificates build-essential musl-tools kmod \
    cpio gzip zstd xz-utils file binutils e2fsprogs fakeroot >/dev/null

# --- The container must BE the guest's architecture --------------------------
# ubuntu:24.04 is multi-arch, so this is an assertion about which host you are
# standing on, not about the image podman fetched. Three later steps quietly
# assume the two agree: apt's dry-run solve and `apt-get download` resolve
# linux-image-generic for the container's own architecture, rustup builds at
# native speed, and verify.txt's version probes actually execute the binaries
# they report on. A mismatch is therefore not a slow build under qemu-user —
# it is an apt failure five minutes in, blamed on the mirror.
NATIVE_ARCH=$(dpkg --print-architecture)
if [ "$NATIVE_ARCH" != "$ARCH" ]; then
  printf 'error: ARCH=%s was asked for, but this container is %s.\n' "$ARCH" "$NATIVE_ARCH" >&2
  printf '%s\n' \
    "Nothing in this build is cross-architecture, by design: the boot media is" \
    "built on the host that will boot it — arm64 on an Apple-silicon Mac," \
    "amd64 on an x86_64 host (a Linux box, or WSL2 on the Windows machine)." \
    "Build the $ARCH media there, or drop ARCH to build for this host." >&2
  exit 1
fi

# --- The two guests' device models, as data ----------------------------------
# One kernel config symbol per driver this guest needs, and the real module
# basename to ship when the pinned kernel package built that symbol as `m`.
# The lists differ because the hardware does (SYNOD.md §2): arm64 sits on
# Virtualization.framework's virtio devices, amd64 on Hyper-V's VMBus. What
# each list must NOT contain is as deliberate as what it does, so the symbol
# we decline is still recorded in kernel-config-check.txt — a reader of the
# artifact can see the decision, not just its outcome.
#
# CHECK_SYMS is what gets recorded; MODNAME is what gets shipped. A symbol in
# the first and not the second is seen and declined on purpose; a name in the
# second that has drifted out from under the kernel is a hard failure in
# step 3, never a silently missing driver.
case "$ARCH" in
  arm64)
    # Virtualization.framework's guest: virtio all the way down — the two
    # disks on virtio-blk, the console on hvc0 (virtio_console), the granted
    # folder as a virtiofs share under vz.rs's own `WORKSPACE_TAG`, and the
    # control plane on the virtio vsock transport. That last one is why
    # CONFIG_HYPERV_VSOCKETS is recorded and declined: shipping it would load
    # a driver that finds no device.
    CHECK_SYMS=(CONFIG_VIRTIO CONFIG_VIRTIO_RING CONFIG_VIRTIO_MMIO CONFIG_VIRTIO_PCI
                CONFIG_VIRTIO_BLK CONFIG_VIRTIO_CONSOLE CONFIG_VIRTIO_FS CONFIG_VSOCKETS
                CONFIG_VIRTIO_VSOCKETS CONFIG_HYPERV_VSOCKETS CONFIG_OVERLAY_FS
                CONFIG_EXT4_FS CONFIG_DEVTMPFS CONFIG_DEVTMPFS_MOUNT)
    # The vsock family's real names (confirmed against this kernel's own
    # modules.dep, not guessed from its Kconfig symbol): the virtio transport
    # is `vmw_vsock_virtio_transport`, not `virtio_vsock`, and it pulls the
    # core `vsock` module in automatically via its own modules.dep line.
    declare -A MODNAME=(
      [CONFIG_VIRTIO]=virtio [CONFIG_VIRTIO_RING]=virtio_ring
      [CONFIG_VIRTIO_MMIO]=virtio_mmio [CONFIG_VIRTIO_PCI]=virtio_pci
      [CONFIG_VIRTIO_BLK]=virtio_blk [CONFIG_VIRTIO_CONSOLE]=virtio_console
      [CONFIG_VSOCKETS]=vsock [CONFIG_VIRTIO_VSOCKETS]=vmw_vsock_virtio_transport
      [CONFIG_VIRTIO_FS]=virtiofs [CONFIG_OVERLAY_FS]=overlay
      [CONFIG_EXT4_FS]=ext4
    )
    BIN_FORMAT="ARM aarch64"
    ;;
  amd64)
    # Hyper-V's guest. `hv_vmbus` IS the bus — nothing else here is reachable
    # until it has enumerated — and on it: `hv_storvsc`, the SCSI HBA behind
    # both disks; `hv_utils`, the KVP / shutdown / timesync services the host
    # expects a well-behaved guest to answer; `hv_balloon`, its dynamic-memory
    # client.
    #
    # There is deliberately NO `hv_netvsc`, though Ubuntu builds it: the guest
    # has no network device at all (§6), so its driver would be code loaded
    # for hardware that is not there. CONFIG_HYPERV_NET is recorded to say
    # that plainly.
    #
    # The control plane is the same AF_VSOCK the guest already speaks (§3);
    # only the transport under it changes, from virtio's to Hyper-V's
    # `hv_sock`. So CONFIG_VIRTIO_VSOCKETS is recorded and declined here for
    # exactly the reason CONFIG_HYPERV_VSOCKETS is declined on arm64.
    #
    # The workspace: there is no virtiofs on Hyper-V. The granted folder
    # arrives as a 9p (9p2000.L) share the host serves over a vsock port, so
    # this guest needs the 9p client (`9p`) over the 9p protocol core
    # (`9pnet`) — plus `9pnet_fd`. That last one is the trap: `trans=fd` used
    # to be compiled inside 9pnet, and in a kernel new enough to have split it
    # out (CONFIG_NET_9P_FD; upstream `9pnet_fd-objs := trans_fd.o`) a mount
    # with 9pnet loaded and 9pnet_fd missing fails with a bare ENODEV. It is
    # in the map, so whichever way this kernel built it, the mount has its
    # transport.
    #
    # SCSI: `scsi_mod` and `sd_mod` are `y` in Ubuntu's amd64 config today, so
    # nothing is shipped for them — but they are named here so that a kernel
    # which makes either a module ships it, resolved from modules.dep like
    # everything else rather than assumed absent forever.
    #
    # The console is the one difference this script does not carry: arm64's
    # machine has hvc0 (virtio_console, above), the Hyper-V machine an
    # emulated COM port on a named pipe at ttyS0. CONFIG_SERIAL_8250 is `y`
    # in Ubuntu's config, so there is no module to ship — and the host writes
    # the kernel command line either way, so this image is neutral about which
    # console it is handed.
    #
    # CONFIG_HYPERV and CONFIG_HYPERV_VMBUS are two different questions, and
    # conflating them cost this script the one module nothing else is reachable
    # without. The first is the umbrella bool for guest support — the arch code
    # under it (CONFIG_HYPERV_TIMER, CONFIG_HYPERV_IOMMU) is `y` and has no
    # module — while the *bus driver* has its own tristate, and in this kernel
    # CONFIG_HYPERV_VMBUS=m. Mapping `hv_vmbus` to the umbrella therefore read
    # its `y` as "built in, nothing to ship", and hv_vmbus reached the guest only
    # because hv_storvsc, hv_utils, hv_balloon and hv_sock each name it on their
    # own modules.dep lines. That is true today and is not a thing to rely on: a
    # config that made those four `y` and left the bus a module would ship a
    # guest that cannot see its disks. Both symbols are recorded, and it is the
    # tristate that carries the module name.
    CHECK_SYMS=(CONFIG_HYPERV CONFIG_HYPERV_VMBUS CONFIG_HYPERV_STORAGE
                CONFIG_HYPERV_UTILS
                CONFIG_HYPERV_BALLOON CONFIG_HYPERV_NET CONFIG_VSOCKETS
                CONFIG_HYPERV_VSOCKETS CONFIG_VIRTIO_VSOCKETS CONFIG_9P_FS
                CONFIG_NET_9P CONFIG_NET_9P_FD CONFIG_SCSI CONFIG_BLK_DEV_SD
                CONFIG_OVERLAY_FS CONFIG_EXT4_FS CONFIG_DEVTMPFS
                CONFIG_DEVTMPFS_MOUNT CONFIG_SERIAL_8250)
    # Real module basenames, as Ubuntu's own linux-modules package ships them
    # (`hv_storvsc`, not `storvsc_drv`; `hv_sock`, not `hyperv_transport`;
    # `9p`, not `9pfs`). `9p` pulls the netfs core in through its own
    # modules.dep line, the way the virtio transport pulls `vsock` in on arm64.
    declare -A MODNAME=(
      [CONFIG_HYPERV_VMBUS]=hv_vmbus [CONFIG_HYPERV_STORAGE]=hv_storvsc
      [CONFIG_HYPERV_UTILS]=hv_utils [CONFIG_HYPERV_BALLOON]=hv_balloon
      [CONFIG_VSOCKETS]=vsock [CONFIG_HYPERV_VSOCKETS]=hv_sock
      [CONFIG_9P_FS]=9p [CONFIG_NET_9P]=9pnet [CONFIG_NET_9P_FD]=9pnet_fd
      [CONFIG_SCSI]=scsi_mod [CONFIG_BLK_DEV_SD]=sd_mod
      [CONFIG_OVERLAY_FS]=overlay [CONFIG_EXT4_FS]=ext4
    )
    BIN_FORMAT="x86-64"
    ;;
esac

# `file`'s own description of a built binary must name the guest's
# architecture and say the link is static: a musl Rust build reports either
# "statically linked" or "static-pie linked" depending on the toolchain, and
# both are static — while a dynamically linked or foreign-arch binary would
# fail only in the guest, where nothing can report it.
is_guest_binary() {
  case "$(file -b "$1")" in
    *"$BIN_FORMAT"*static*) return 0 ;;
    *) return 1 ;;
  esac
}

# --- 1. Cross-build ral-daemon, exarch, ral-initramfs -----------------------
# Same recipe as build-binaries.yml's musl jobs (build-exarch-linux-arm64 for
# aarch64, its x86_64 siblings for amd64): rustup + musl-tools, no cross-rs
# container. /ral is read-only, so the target dir and cargo home move off it.
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
for b in ral-daemon exarch ral-initramfs; do
  is_guest_binary "$BIN/$b" \
    || { echo "error: $b is not a static $BIN_FORMAT binary: $(file -b "$BIN/$b")" >&2; exit 1; }
done

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

# The two loaders want the kernel in two different shapes, and the asymmetry
# below is not an oversight but the whole of the difference:
#
#   arm64 / VZLinuxBootLoader wants a raw uncompressed ARM64 `Image`, and
#     Ubuntu ships vmlinuz wrapped — a unified-image PE whose kernel is its
#     .linux section, an EFI zboot PE ("zimg" magic at byte 4, LE32 payload
#     offset/size at bytes 8/12), and a compressed payload, nested per suite —
#     so the layers are peeled, one at a time, refusing anything unknown.
#
#   amd64 / Hyper-V's LinuxKernelDirect wants a bzImage, and Ubuntu's
#     vmlinuz-*-generic for amd64 already IS a bzImage. So that path unwraps
#     NOTHING: peeling here would produce a bare ELF vmlinux the loader cannot
#     start. The bytes are copied through and asserted, not transformed.
#
# The helpers below therefore serve the arm64 path only.
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

KERNEL_PKG_VERSION=$(dpkg-deb -f /build/kernel-debs/"$IMAGE_PKG"_*.deb Version | head -1)

# --- 3. Confirm the module set against the real kernel config ---------------
# README.md's prose named a baseline; this is the pass that checks it against
# what the pinned kernel package actually built, for whichever guest's table
# was selected above. y = compiled in (nothing to ship); m = a module this
# initramfs must carry and load; missing = a symbol this kernel does not have
# at all, which is worth seeing in the artifact.
{
  echo "# boot.img kernel config check — $KVER ($ARCH)"
  for sym in "${CHECK_SYMS[@]}"; do
    grep -E "^$sym=" "$CONFIG_PATH" || echo "$sym is not set"
  done
} > /out/kernel-config-check.txt
cat /out/kernel-config-check.txt

# Only the `m` entries of this guest's MODNAME table become cargo: a `y` needs
# nothing shipped, and a symbol recorded above but absent from the table is
# one this guest declines on purpose.
#
# Sorted, because `${!MODNAME[@]}` walks a bash hash table: leaving the order to
# it shuffles the `modules_shipped=` line of boot-manifest.txt between builds of
# identical inputs, and that file exists to be compared.
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

  # Every module this boot will hold: the requested ones, plus each module named
  # on their dependency lines, plus — recorded for all of them alike — what that
  # module itself depends on.  A dependency's own line is looked up too, because
  # the sort below needs the graph, not just the roots.
  declare -A DEPS_OF=()
  CLOSURE=()
  for name in "${NEEDED_MODULES[@]}"; do
    line=$(grep -m1 "^[^:]*/${name}\.ko[^:]*:" "$MODDEP" || true)
    if [ -z "$line" ]; then
      # =m in the config but absent from modules.dep: the MODNAME table has
      # drifted from the kernel, and the boot artifact would fail at mount.
      echo "error: $name is =m in the kernel config but not in modules.dep" >&2
      exit 1
    fi
    # A leaf module's line ends right at the colon with no trailing space
    # (e.g. "vsock.ko.zst:"); requiring "*: " here would leave the dependency
    # list as the whole "target:" string instead of empty.  Unquoted expansion
    # then contributes nothing for a leaf, and ignores the one leading space a
    # non-empty list begins with.
    for modpath in "${line%%:*}" ${line#*:}; do
      [ -n "${DEPS_OF[$modpath]+recorded}" ] && continue
      own=$(grep -m1 "^${modpath}:" "$MODDEP")
      DEPS_OF[$modpath]="${own#*:}"
      CLOSURE+=("$modpath")
    done
  done

  # A load order, computed rather than read off the file.  Each modules.dep line
  # does list a module's full transitive dependencies — but as a *set*, in no
  # particular order: this kernel writes `9pnet_fd: 9pnet netfs`, and `9pnet`
  # needs symbols `netfs` exports, so inserting them as written kills the guest
  # a tenth of a second into its boot.  Emitting only what has all its
  # dependencies already emitted cannot make that mistake.  Module dependencies
  # are a DAG, so a pass that emits nothing means this kernel shipped a cycle —
  # not something to paper over, since the guest could not boot on it either.
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
cp "$BIN/ral-initramfs" /build/cpio/init
cp "$BIN/ral-daemon" /build/cpio/ral-daemon
cp "$BIN/exarch" /build/cpio/engine
chmod 0755 /build/cpio/init /build/cpio/ral-daemon /build/cpio/engine /build/cpio/sbin/mke2fs
# The node the kernel opens as init's stdio; /init mounts devtmpfs over
# /dev itself before it needs any device beyond the console.  Rootless
# podman has no CAP_MKNOD, so the node and the archive that records it are
# both made under fakeroot, which fabricates the char device honestly
# without privilege — the same tool dpkg uses to build device nodes.
echo ">> [container] assembling initramfs.img"
fakeroot sh -c '
  mkdir -p /build/cpio/dev
  mknod -m 600 /build/cpio/dev/console c 5 1
  cd /build/cpio && find . | cpio -o -H newc 2>/dev/null | gzip -9
' > /out/initramfs.img

( cd /out && sha256sum kernel > kernel.sha256 && sha256sum initramfs.img > initramfs.img.sha256 )

# --- 6. Verify without booting ------------------------------------------------
{
  echo "# boot.img verification — $SUITE/$ARCH, kernel $KVER"
  echo
  echo "== kernel: $KERNEL_NOTE =="
  file /out/kernel
  echo
  echo "== ral-daemon / exarch / ral-initramfs: static, $BIN_FORMAT =="
  file "$BIN/ral-daemon" "$BIN/exarch" "$BIN/ral-initramfs"
  for b in ral-daemon exarch ral-initramfs; do
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
  for path in init ral-daemon engine sbin/mke2fs modules.order dev/console; do
    [ -e "/build/cpio/$path" ] && echo "OK   /$path" || echo "MISSING /$path"
  done
  [ -d /build/cpio/modules ] && echo "OK   /modules (${#NEEDED_MODULES[@]} driver(s) requested)"
  echo
  # These really run: the container is $NATIVE_ARCH, asserted equal to $ARCH
  # before anything was built, so no qemu stands between this shell and the
  # guest's own binaries.
  echo "== version probes (native $NATIVE_ARCH container, no qemu) =="
  /build/cpio/sbin/mke2fs -V 2>&1 | head -1
  "$BIN/ral-daemon" --help >/dev/null 2>&1 || echo "ral-daemon: refuses off pid 1, as designed"
  "$BIN/exarch" --version 2>&1 | head -1 || true
} > /out/verify.txt
cat /out/verify.txt

# arch first and alone on its line: out/boot/ does not encode the guest in any
# path, so this is the one place that says which machine the media beside it
# will boot.
{
  echo "arch=$ARCH"
  echo "kernel_format=$KERNEL_NOTE"
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
