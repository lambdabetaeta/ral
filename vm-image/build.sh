#!/usr/bin/env bash
#
# Build the synod guest rootfs image: an Ubuntu 26.04 LTS (resolute) arm64
# "office userland" ext4 image (SYNOD.md §7). The guest has no network, so this
# image is the whole package manager — everything office work needs must already
# be inside it.
#
# The entire build runs inside one native-arm64 ubuntu:24.04 container (macOS
# has no mmdebstrap or ext4 tooling). Inside it: mmdebstrap assembles the rootfs
# tree from packages.txt, then `mkfs.ext4 -d` turns that directory straight into
# an image with no loop mount and no host privileges on the *image* step. The
# container itself is --privileged only because mmdebstrap's root mode and the
# chroot spot-checks bind-mount /proc,/sys,/dev.
#
# Outputs (in vm-image/out/, on the Mac via a bind mount):
#   rootfs.img               the ext4 image
#   rootfs.img.sha256        its checksum
#   packages-manifest.txt    dpkg -l capture — the exact version pin record
#   verify.txt               spot-check results
#
# The multi-GB tree and apt cache live only in the container's ephemeral layer
# and are reclaimed when it exits (--rm). Nothing is left on the podman machine.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT_DIR="$SCRIPT_DIR/out"

# --- Knobs (env-overridable) -------------------------------------------------
# arm64 lives on ports.ubuntu.com, NOT archive.ubuntu.com. For bit-for-bit
# reproducibility, point MIRROR at a snapshot.ubuntu.com timestamp instead; the
# live ports mirror is used by default for reliability, and packages-manifest.txt
# records the exact versions that were pulled either way.
SUITE="${SUITE:-resolute}"   # Ubuntu 26.04 LTS (the current LTS)
MIRROR="${MIRROR:-http://ports.ubuntu.com/ubuntu-ports}"
ARCH="${ARCH:-arm64}"
BASE_IMAGE="${BASE_IMAGE:-docker.io/library/ubuntu:24.04}"
# rootfs.img is the RO overlay LOWER layer (§7): immutable at the block layer,
# it never grows — all guest writes land in the overlay UPPER on session.img.
# So the image is sized SNUGLY: mkfs at a generous size, then `resize2fs -M`
# shrinks it to the smallest size that holds its content. No overlay "breathing
# room"; free space here would be pure dead weight in the download.
ZSTD_LEVEL="${ZSTD_LEVEL:-19}"           # download artifact compression
# Fixed FS identity => the image bytes do not churn on rebuild.
FS_UUID="${FS_UUID:-5d9e0b7a-6f2c-4e13-9a44-73b6c0f1a201}"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-1753056000}"  # 2026-07-21, pinned

mkdir -p "$OUT_DIR"
# Export so `podman run -e NAME` pass-through actually finds them.
export SUITE MIRROR ARCH ZSTD_LEVEL FS_UUID SOURCE_DATE_EPOCH

command -v podman >/dev/null || { echo "podman not found" >&2; exit 1; }

echo ">> building $SUITE/$ARCH office rootfs via $BASE_IMAGE"
echo ">> mirror: $MIRROR"
START=$(date +%s)

podman run --rm -i \
  --privileged \
  --security-opt label=disable \
  -e SUITE -e MIRROR -e ARCH -e ZSTD_LEVEL -e FS_UUID -e SOURCE_DATE_EPOCH \
  -v "$OUT_DIR:/out" \
  -v "$SCRIPT_DIR/packages.txt:/packages.txt:ro" \
  "$BASE_IMAGE" \
  bash -euo pipefail -s <<'INNER'
export DEBIAN_FRONTEND=noninteractive

echo ">> [container] installing build tooling"
apt-get update -qq
apt-get install -y -qq --no-install-recommends mmdebstrap e2fsprogs zstd >/dev/null

# Package list: strip comments and blanks, comma-join for mmdebstrap --include.
INCLUDE=$(grep -v '^#' /packages.txt | grep -v '^[[:space:]]*$' | paste -sd,)
echo ">> [container] $(tr ',' '\n' <<<"$INCLUDE" | wc -l) packages requested"

# --- 1. Assemble the rootfs tree --------------------------------------------
# --variant=minbase : Priority:required + apt only. Not 'important'/'standard'
#   (those pull an ubuntu-minimal-ish set we do not want) and emphatically not
#   'buildd' (that is build-essential). This is the leanest base that still has
#   a real userland (passwd, util-linux, base-files).
# Recommends OFF, deliberately: it is the channel through which compilers, -dev
#   headers and desktop/X integration would sneak in. Everything actually needed
#   is named explicitly in packages.txt; verify.txt asserts no toolchain landed.
# components=main,universe : csvkit, ripgrep, tesseract language data and the
#   metric-compatible fonts live in universe.
echo ">> [container] mmdebstrap: assembling tree"
mkdir -p /build
mmdebstrap \
  --architectures="$ARCH" \
  --variant=minbase \
  --mode=root \
  --components='main,universe' \
  --aptopt='Apt::Install-Recommends "false"' \
  --include="$INCLUDE" \
  "$SUITE" \
  /build/rootfs \
  "$MIRROR"

# Pre-build the fontconfig cache INTO the image. Without it the first fontconfig
# consumer in the guest (LibreOffice, ImageMagick, any renderer) pays a cold scan
# over the whole wide font set on first boot — dominated by the Noto CJK faces,
# and measured here at minutes. Baking /var/cache/fontconfig makes first use fast.
echo ">> [container] pre-building fontconfig cache"
chroot /build/rootfs /usr/bin/env -i PATH=/usr/bin:/bin fc-cache -f

TREE_BYTES=$(du -sb /build/rootfs | cut -f1)
NFILES=$(find /build/rootfs -xdev | wc -l)
echo ">> [container] tree: $TREE_BYTES bytes (apparent), $NFILES filesystem objects"

# --- 2. Make the ext4 image from the directory, then minimize it ------------
# mkfs at a size known to be generous enough to hold everything; resize2fs -M
# then shrinks it to the smallest size the content needs. Sizing is thus
# determined, not guessed.
INODES=$(( NFILES + NFILES / 5 + 8192 ))            # ~20% inode slack
GEN_BYTES=$(( TREE_BYTES * 3 / 2 + 536870912 ))     # apparent x1.5 + 512MiB
GEN_BYTES=$(( (GEN_BYTES + 4095) / 4096 * 4096 ))
truncate -s "$GEN_BYTES" /out/rootfs.img
# Fixed UUID + hash_seed and eager table init => stable, non-churning bytes.
mkfs.ext4 -q -F -t ext4 -b 4096 \
  -L synod-rootfs \
  -U "$FS_UUID" \
  -E "hash_seed=$FS_UUID,lazy_itable_init=0,lazy_journal_init=0" \
  -N "$INODES" \
  -d /build/rootfs \
  /out/rootfs.img

echo ">> [container] minimizing image (resize2fs -M)"
e2fsck -fy /out/rootfs.img >/dev/null 2>&1 || true
resize2fs -M /out/rootfs.img >/dev/null 2>&1
BLK=$(dumpe2fs -h /out/rootfs.img 2>/dev/null | awk -F: '/Block count/{gsub(/ /,"",$2);print $2}')
BS=$(dumpe2fs -h /out/rootfs.img 2>/dev/null | awk -F: '/Block size/{gsub(/ /,"",$2);print $2}')
SIZE_BYTES=$(( BLK * BS ))
truncate -s "$SIZE_BYTES" /out/rootfs.img
echo ">> [container] snug image: $SIZE_BYTES bytes ($BLK x $BS blocks)"

# --- 3. Download artifact, checksums, version-pin manifest ------------------
# The hypervisor mounts the raw .img; users download the compressed .zst.
echo ">> [container] compressing rootfs.img.zst (zstd -$ZSTD_LEVEL)"
zstd -q -"$ZSTD_LEVEL" -T0 --long=27 -f /out/rootfs.img -o /out/rootfs.img.zst
IMG_BYTES=$(stat -c%s /out/rootfs.img)
ZST_BYTES=$(stat -c%s /out/rootfs.img.zst)
echo ">> [container] raw=$IMG_BYTES zst=$ZST_BYTES (ratio $(awk "BEGIN{printf \"%.2f\",$IMG_BYTES/$ZST_BYTES}"))"

( cd /out && sha256sum rootfs.img > rootfs.img.sha256 \
          && sha256sum rootfs.img.zst > rootfs.img.zst.sha256 )

COLUMNS=200 dpkg-query --admindir=/build/rootfs/var/lib/dpkg -l > /out/packages-manifest.txt
PKG_COUNT=$(dpkg-query --admindir=/build/rootfs/var/lib/dpkg -W -f='.\n' | wc -l)
echo ">> [container] installed packages: $PKG_COUNT"

# --- 4. Spot-checks (chroot into the tree; the image is already built) ------
{
  echo "# synod rootfs verification"
  echo "suite=$SUITE arch=$ARCH mirror=$MIRROR"
  echo "tree_usage=$TREE_BYTES image_bytes=$IMG_BYTES zst_bytes=$ZST_BYTES fs_objects=$NFILES inodes=$INODES pkg_count=$PKG_COUNT"
  echo
} > /out/verify.txt

for d in proc sys dev; do mount --bind "/$d" "/build/rootfs/$d"; done
run() { chroot /build/rootfs /usr/bin/env -i HOME=/tmp PATH=/usr/bin:/bin timeout 180 "$@"; }

set +e
echo "== python document stack ==" >> /out/verify.txt
run python3 -c "import pandas, openpyxl, docx, pypdf, PIL; print('py imports OK:', pandas.__version__, PIL.__version__)" >> /out/verify.txt 2>&1

echo "== pandoc ==" >> /out/verify.txt
run pandoc --version 2>&1 | head -1 >> /out/verify.txt
echo '# Hi' | run pandoc -f markdown -t html >> /out/verify.txt 2>&1

echo "== libreoffice headless (version from manifest + txt->pdf convert) ==" >> /out/verify.txt
dpkg-query --admindir=/build/rootfs/var/lib/dpkg -W \
  -f='libreoffice-core ${Version}\n' libreoffice-core >> /out/verify.txt 2>&1
printf 'synod headless convert smoke test\n' > /build/rootfs/tmp/smoke.txt
# soffice.bin catches SIGTERM and lingers, so plain `timeout` cannot reap it:
# -k follows with SIGKILL. A private UserInstallation gives this run its own
# profile lock, avoiding the contention that otherwise hangs the convert.
# The ceiling is generous: soffice cold-start in this constrained build
# container is slow (fontconfig is pre-cached, but profile bootstrap is not).
chroot /build/rootfs /usr/bin/env -i HOME=/tmp PATH=/usr/bin:/bin \
  timeout -k 15 300 soffice --headless \
    -env:UserInstallation=file:///tmp/lo-verify \
    --convert-to pdf --outdir /tmp /tmp/smoke.txt >/dev/null 2>&1 || true
if [ -f /build/rootfs/tmp/smoke.pdf ]; then
  echo "txt->pdf convert: OK ($(stat -c%s /build/rootfs/tmp/smoke.pdf) bytes)" >> /out/verify.txt
else
  echo "txt->pdf convert: FAILED (non-fatal spot-check)" >> /out/verify.txt
fi
rm -rf /build/rootfs/tmp/lo-verify

echo "== tesseract languages ==" >> /out/verify.txt
run tesseract --list-langs >> /out/verify.txt 2>&1

echo "== other tools ==" >> /out/verify.txt
run qpdf --version 2>&1 | head -1 >> /out/verify.txt
run rg --version 2>&1 | head -1 >> /out/verify.txt
run ocrmypdf --version 2>&1 | head -1 >> /out/verify.txt

echo "== toolchain must be ABSENT ==" >> /out/verify.txt
if dpkg-query --admindir=/build/rootfs/var/lib/dpkg -W \
     -f='${Package}\n' 2>/dev/null \
   | grep -Ex '(gcc|gcc-[0-9]+|g\+\+|cpp|make|build-essential|dpkg-dev|binutils|libc6-dev)'; then
  echo "WARNING: toolchain package present (see above)" >> /out/verify.txt
else
  echo "clean: no gcc/g++/make/build-essential/dpkg-dev/binutils/libc6-dev" >> /out/verify.txt
fi
set -e

for d in proc sys dev; do umount "/build/rootfs/$d"; done
rm -f /build/rootfs/tmp/smoke.txt /build/rootfs/tmp/smoke.pdf

echo ">> [container] done"
INNER

END=$(date +%s)
ELAPSED=$(( END - START ))

img_mib() { echo $(( ( $(stat -f%z "$1" 2>/dev/null || stat -c%s "$1") ) / 1024 / 1024 )); }
echo
echo ">> BUILD COMPLETE in ${ELAPSED}s"
echo ">> image (mounted by hypervisor): $OUT_DIR/rootfs.img ($(img_mib "$OUT_DIR/rootfs.img") MiB)"
echo ">> download artifact           : $OUT_DIR/rootfs.img.zst ($(img_mib "$OUT_DIR/rootfs.img.zst") MiB)"
echo ">> sha256: $(cat "$OUT_DIR/rootfs.img.sha256")"
echo ">> manifest + verification in $OUT_DIR/"
