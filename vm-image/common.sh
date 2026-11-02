# shellcheck shell=bash
# Shared skeleton of build-rootfs.sh and build-boot.sh; sourced, never run.
# Both build inside one $BASE_IMAGE container of the guest's own architecture.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

SUITE="${SUITE:-resolute}"
BASE_IMAGE="${BASE_IMAGE:-docker.io/library/ubuntu:24.04}"
# The guest is the host's own architecture: nothing here cross-builds. The two
# mirrors do not serve each other; an explicit MIRROR (a snapshot.ubuntu.com
# timestamp) wins. https, because mmdebstrap bakes it into the guest's
# sources.list and the guest's only door out is a CONNECT-only proxy on 443.
case "$(uname -m)" in
  arm64 | aarch64)
    ARCH=arm64 RUST_TARGET=aarch64-unknown-linux-musl
    : "${MIRROR:=https://ports.ubuntu.com/ubuntu-ports}" ;;
  x86_64)
    ARCH=amd64 RUST_TARGET=x86_64-unknown-linux-musl
    : "${MIRROR:=https://archive.ubuntu.com/ubuntu}" ;;
  *) echo "error: no synod guest for a $(uname -m) host (SYNOD.md §2): build on an" >&2
     echo "Apple-silicon Mac (arm64, Virtualization.framework) or an x86_64 Linux" >&2
     echo "box or WSL2 (amd64, Hyper-V)." >&2
     exit 1 ;;
esac

GIT_HASH="$(git -C "$REPO_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
export SUITE ARCH RUST_TARGET MIRROR GIT_HASH

# Refuse unless podman runs containers of the host's own $ARCH: both
# builds execute guest binaries (chroot spot-checks, version probes), and apt
# resolves the kernel package for the container's own architecture.
preflight() {
  command -v podman >/dev/null \
    || { echo "error: podman not found; both builds run inside a podman container." >&2; exit 1; }
  local native
  native="$(podman run --rm "$BASE_IMAGE" dpkg --print-architecture)" \
    || { echo "error: could not run $BASE_IMAGE under podman; is the podman machine up?" >&2; exit 1; }
  [ "$native" = "$ARCH" ] || {
    echo "error: this is a $ARCH host, but podman runs $native containers (an emulating podman machine?)." >&2
    echo "Nothing in these builds is cross-architecture; recreate the podman machine for $ARCH." >&2
    exit 1
  }
}

# stage <name>: a private staging dir (concurrent builds never share one) for
# out/<name>, cleaned on exit and swapped in by `publish`.
stage() {
  OUT_DIR="$SCRIPT_DIR/out/$1"
  mkdir -p "$(dirname "$OUT_DIR")"
  STAGING="$(mktemp -d "$OUT_DIR.staging.XXXXXX")"
  chmod 755 "$STAGING"
  trap 'rm -rf "$STAGING"; [ -z "${LOCKED:-}" ] || rmdir "$OUT_DIR.lock"' EXIT
  START="$(date +%s)"
}

# run_container [podman flags]: bash on stdin runs in the build container with
# $STAGING at /out; the transcript lands in $STAGING/build.log.
run_container() {
  podman run --rm -i --privileged --security-opt label=disable \
    -e SUITE -e ARCH -e RUST_TARGET -e MIRROR -e GIT_HASH \
    -v "$STAGING:/out" "$@" "$BASE_IMAGE" \
    bash -euo pipefail -s 2>&1 | tee "$STAGING/build.log"
}

# publish <file>: swap $STAGING into $OUT_DIR once <file> carries this build's
# stamp. podman can exit 0 after the inner script died (full VM disk).
publish() {
  grep -qx "ral_git_hash=$GIT_HASH" "$STAGING/$1" 2>/dev/null || {
    echo "error: the build container exited without finishing; the previous media in $OUT_DIR is untouched. The build log is above." >&2
    exit 1
  }
  until mkdir "$OUT_DIR.lock" 2>/dev/null; do sleep 1; done
  LOCKED=1
  rm -rf "$OUT_DIR"
  mv "$STAGING" "$OUT_DIR"
  echo
  echo ">> BUILD COMPLETE in $(( $(date +%s) - START ))s"
}
