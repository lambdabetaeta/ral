#!/usr/bin/env bash
# Build rootfs.img: the synod guest's Ubuntu office userland (README.md), as a
# raw ext4 image in vm-image/out/rootfs/ (also .zst, .sha256, packages-manifest.txt,
# verify.txt, build.log). Raw on both arches: the Windows backend wraps it as a
# fixed VHD at first boot, not here. The guest has no network: this image is the
# whole package manager.
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

ZSTD_LEVEL="${ZSTD_LEVEL:-19}"
# Pinned, not knobs: identical inputs must yield identical bytes.
FS_UUID=5d9e0b7a-6f2c-4e13-9a44-73b6c0f1a201
SOURCE_DATE_EPOCH=1753056000  # 2026-07-21
export ZSTD_LEVEL FS_UUID SOURCE_DATE_EPOCH

preflight
stage rootfs
echo ">> building $SUITE/$ARCH office rootfs via $BASE_IMAGE"
echo ">> mirror: $MIRROR"

# privileged: mmdebstrap's root mode and the chroot checks bind-mount /proc,/sys,/dev.
run_container \
  -e ZSTD_LEVEL -e FS_UUID -e SOURCE_DATE_EPOCH \
  -v "$SCRIPT_DIR/packages.txt:/packages.txt:ro" <<'INNER'
export DEBIAN_FRONTEND=noninteractive

echo ">> [container] installing build tooling"
apt-get update -qq
# ca-certificates: the base image has no CA bundle, and MIRROR is https.
apt-get install -y -qq --no-install-recommends \
  mmdebstrap e2fsprogs zstd ca-certificates >/dev/null

# Package list: strip comments and blanks, comma-join for mmdebstrap --include.
INCLUDE=$(grep -v '^#' /packages.txt | grep -v '^[[:space:]]*$' | paste -sd,)
echo ">> [container] $(tr ',' '\n' <<<"$INCLUDE" | wc -l) packages requested"

# --- 1. Assemble the rootfs tree --------------------------------------------
# minbase, Recommends off and main+universe: rationale in README.
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

# Bake the fontconfig cache: a cold scan of the Noto CJK faces costs minutes at first boot.
echo ">> [container] pre-building fontconfig cache"
chroot /build/rootfs /usr/bin/env -i PATH=/usr/bin:/bin fc-cache -f

TREE_BYTES=$(du -sb /build/rootfs | cut -f1)
NFILES=$(find /build/rootfs -xdev | wc -l)
echo ">> [container] tree: $TREE_BYTES bytes (apparent), $NFILES filesystem objects"

# --- 2. Make the ext4 image from the directory, then minimize it ------------
# mkfs generously, then resize2fs -M: rootfs.img is the RO overlay lower layer, so free space is dead weight.
INODES=$(( NFILES + NFILES / 5 + 8192 ))            # ~20% inode slack
GEN_BYTES=$(( TREE_BYTES * 3 / 2 + 536870912 ))     # apparent x1.5 + 512MiB
GEN_BYTES=$(( (GEN_BYTES + 4095) / 4096 * 4096 ))
truncate -s "$GEN_BYTES" /out/rootfs.img
# Fixed UUID + hash_seed and eager init keep the bytes stable across rebuilds.
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
echo ">> [container] compressing rootfs.img.zst (zstd -$ZSTD_LEVEL)"
# --long=27 is `vm_manager::media::ROOTFS_WINDOW`: raise both together.
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
# soffice.bin survives SIGTERM, hence -k; a private UserInstallation avoids profile-lock hangs; cold start here is slow.
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

echo "ral_git_hash=$GIT_HASH" >> /out/verify.txt
echo ">> [container] done"
INNER

publish verify.txt

img_mib() { echo $(( $(stat -c%s "$1" 2>/dev/null || stat -f%z "$1") / 1024 / 1024 )); }
echo ">> image (mounted by hypervisor): $OUT_DIR/rootfs.img ($(img_mib "$OUT_DIR/rootfs.img") MiB)"
echo ">> download artifact           : $OUT_DIR/rootfs.img.zst ($(img_mib "$OUT_DIR/rootfs.img.zst") MiB)"
echo ">> sha256: $(cat "$OUT_DIR/rootfs.img.sha256")"
echo ">> manifest + verification in $OUT_DIR/"
