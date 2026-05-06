#!/usr/bin/env bash
# build-release.sh — local replacement for .github/workflows/build-binaries.yml.
#
# Builds the release matrix locally and publishes the artifacts to the `latest`
# prerelease on GitHub via `gh`.  Used when GitHub Actions minutes are
# exhausted.
#
# Targets built by default (all inside Docker):
#   ral-linux-x86_64           (x86_64-unknown-linux-musl)
#   ral-linux-x86_64-allutils  (x86_64-unknown-linux-musl, +coreutils,diffutils,grep)
#   ral-linux-arm64            (aarch64-unknown-linux-musl)
#   ral-linux-arm64-allutils   (aarch64-unknown-linux-musl, +coreutils,diffutils,grep)
#   ral-windows.exe            (x86_64-pc-windows-gnu)
#   ral-windows-allutils.exe   (x86_64-pc-windows-gnu, +coreutils,diffutils,grep)
#
# macOS arm64 cannot be produced from a Linux Docker image without osxcross.
# Pass --macos to additionally build macOS artifacts using the host toolchain
# (this is the one release-time exception to the "no cargo on the host" rule).
#
# Flags:
#   --macos          also build ral-macos-arm64{,-allutils} using host cargo
#   --skip-linux     do not build Linux musl targets
#   --skip-windows   do not build the Windows target
#   --skip-publish   build artifacts only, skip tag move + gh release
#   --dry-run        print what would happen, do not run

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

BUILD_MACOS=0
SKIP_LINUX=0
SKIP_WINDOWS=0
SKIP_PUBLISH=0
DRY_RUN=0

for arg in "$@"; do
    case "$arg" in
        --macos)        BUILD_MACOS=1 ;;
        --skip-linux)   SKIP_LINUX=1 ;;
        --skip-windows) SKIP_WINDOWS=1 ;;
        --skip-publish) SKIP_PUBLISH=1 ;;
        --dry-run)      DRY_RUN=1 ;;
        -h|--help)
            sed -n '2,30p' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

run() {
    if [[ "$DRY_RUN" == 1 ]]; then
        printf '+ %s\n' "$*"
    else
        printf '+ %s\n' "$*"
        "$@"
    fi
}

# Cross-compile images.  The `linux/amd64` platform is pinned because the
# musl-cross and cross-rs images ship amd64 only; on an arm64 host (Apple
# Silicon) Docker Desktop will emulate under Rosetta.
PLATFORM="--platform=linux/amd64"
IMG_MUSL_X86_64="messense/rust-musl-cross:x86_64-musl"
IMG_MUSL_ARM64="messense/rust-musl-cross:aarch64-musl"
IMG_WIN="ghcr.io/cross-rs/x86_64-pc-windows-gnu:main"

ARTIFACT_DIR="$REPO_ROOT/target/release-artifacts"
CACHE_DIR="$REPO_ROOT/target/release-cache"

mkdir -p "$ARTIFACT_DIR" "$CACHE_DIR"
# Separate target dir per toolchain avoids cache thrash across wildly
# different sysroots.  Registry is shared.
mkdir -p \
    "$CACHE_DIR/registry" \
    "$CACHE_DIR/target-x86_64-musl" \
    "$CACHE_DIR/target-aarch64-musl" \
    "$CACHE_DIR/target-x86_64-windows-gnu" \
    "$CACHE_DIR/target-macos"

# Build one artifact inside a docker image.
#   $1 image
#   $2 target triple
#   $3 cache subdir under $CACHE_DIR (target dir)
#   $4 cargo feature string (may be empty)
#   $5 path of built binary relative to cargo target dir
#   $6 output artifact name under $ARTIFACT_DIR
docker_build() {
    local image="$1" triple="$2" cache_subdir="$3" features="$4" bin_rel="$5" out_name="$6"
    local feat_args=()
    [[ -n "$features" ]] && feat_args=(--features "$features")

    echo
    echo "==> $out_name ($triple)"
    run docker run --rm $PLATFORM \
        -v "$REPO_ROOT":/work \
        -v "$CACHE_DIR/registry":/usr/local/cargo/registry \
        -v "$CACHE_DIR/$cache_subdir":/work/target \
        -w /work \
        "$image" \
        cargo build --release -p ral --target "$triple" "${feat_args[@]}"

    if [[ "$DRY_RUN" != 1 ]]; then
        cp "$CACHE_DIR/$cache_subdir/$triple/release/$bin_rel" "$ARTIFACT_DIR/$out_name"
        chmod +x "$ARTIFACT_DIR/$out_name" 2>/dev/null || true
    fi
}

if [[ "$SKIP_LINUX" != 1 ]]; then
    docker_build "$IMG_MUSL_X86_64" x86_64-unknown-linux-musl  target-x86_64-musl  ""                             ral     ral-linux-x86_64
    docker_build "$IMG_MUSL_X86_64" x86_64-unknown-linux-musl  target-x86_64-musl  "coreutils,diffutils,grep"     ral     ral-linux-x86_64-allutils
    docker_build "$IMG_MUSL_ARM64"  aarch64-unknown-linux-musl target-aarch64-musl ""                             ral     ral-linux-arm64
    docker_build "$IMG_MUSL_ARM64"  aarch64-unknown-linux-musl target-aarch64-musl "coreutils,diffutils,grep"     ral     ral-linux-arm64-allutils
fi

if [[ "$SKIP_WINDOWS" != 1 ]]; then
    docker_build "$IMG_WIN" x86_64-pc-windows-gnu target-x86_64-windows-gnu ""                         ral.exe ral-windows.exe
    docker_build "$IMG_WIN" x86_64-pc-windows-gnu target-x86_64-windows-gnu "coreutils,diffutils,grep" ral.exe ral-windows-allutils.exe
fi

if [[ "$BUILD_MACOS" == 1 ]]; then
    if [[ "$(uname -s)" != "Darwin" ]]; then
        echo "--macos requested but host is $(uname -s); skipping" >&2
    else
        host_triple="$(uname -m)"
        case "$host_triple" in
            arm64|aarch64) out_suffix="arm64" ;;
            x86_64)        out_suffix="x86_64" ;;
            *) echo "unsupported macOS host arch $host_triple" >&2; exit 2 ;;
        esac

        echo
        echo "==> ral-macos-$out_suffix (host cargo — release exception to docker-only rule)"
        run env CARGO_TARGET_DIR="$CACHE_DIR/target-macos" \
            cargo build --release -p ral
        if [[ "$DRY_RUN" != 1 ]]; then
            cp "$CACHE_DIR/target-macos/release/ral" "$ARTIFACT_DIR/ral-macos-$out_suffix"
            codesign -s - "$ARTIFACT_DIR/ral-macos-$out_suffix" 2>/dev/null || true
        fi

        echo
        echo "==> ral-macos-$out_suffix-allutils (host cargo)"
        run env CARGO_TARGET_DIR="$CACHE_DIR/target-macos" \
            cargo build --release -p ral --features coreutils,diffutils,grep
        if [[ "$DRY_RUN" != 1 ]]; then
            cp "$CACHE_DIR/target-macos/release/ral" "$ARTIFACT_DIR/ral-macos-$out_suffix-allutils"
            codesign -s - "$ARTIFACT_DIR/ral-macos-$out_suffix-allutils" 2>/dev/null || true
        fi
    fi
fi

# Checksums.
if [[ "$DRY_RUN" != 1 ]]; then
    echo
    echo "==> sha256 checksums"
    ( cd "$ARTIFACT_DIR" && for f in *; do
        [[ -f "$f" && "$f" != *.sha256 ]] || continue
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "$f" | awk '{print $1}' > "$f.sha256"
        else
            shasum -a 256 "$f" | awk '{print $1}' > "$f.sha256"
        fi
    done )
    ls -1 "$ARTIFACT_DIR"
fi

if [[ "$SKIP_PUBLISH" == 1 ]]; then
    echo
    echo "Skipping publish (--skip-publish).  Artifacts in $ARTIFACT_DIR"
    exit 0
fi

echo
echo "==> moving 'latest' tag to HEAD"
run git tag -f latest
run git push origin latest --force

echo
echo "==> replacing 'latest' release via gh"
# Delete first so the title/notes/prerelease flag are refreshed cleanly.
if [[ "$DRY_RUN" != 1 ]]; then
    if gh release view latest >/dev/null 2>&1; then
        gh release delete latest --yes --cleanup-tag=false
    fi
fi

files=()
if [[ "$DRY_RUN" != 1 ]]; then
    shopt -s nullglob
    for f in "$ARTIFACT_DIR"/*; do files+=("$f"); done
    shopt -u nullglob
fi

run gh release create latest \
    --title "Latest Build" \
    --prerelease \
    --notes "Built locally $(date -u +%Y-%m-%dT%H:%M:%SZ) from $(git rev-parse --short HEAD)" \
    "${files[@]}"

echo
echo "Done.  https://github.com/lambdabetaeta/ral-private/releases/tag/latest"
