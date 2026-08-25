#!/bin/sh
# ci.sh — the one CI step list.  GitHub Actions runs it (.github/workflows/ci.yml),
# `just ci` runs it before a push, `just linux-ci` runs it in the container.
# POSIX sh and cargo only, on purpose: CI must not depend on ral building.
#
# The mode picks only where cargo runs.
#   native     this host's toolchain — on macOS the only way to exercise
#              Seatbelt, or to compile synod, at all.
#   linux-box  inside scripts/linux-box.Dockerfile's image — the only way a
#              macOS host compiles the cfg(target_os = "linux") sandbox,
#              which core/src/sandbox.rs gates out of a native build.
set -eu

MODE=${1:-}
case $MODE in
native | linux-box) ;;
*)
    echo 'usage: scripts/ci.sh native|linux-box' >&2
    exit 2
    ;;
esac

cd "$(dirname "$0")/.."

# What CI lints and builds with.  Clippy takes no `-- -D warnings`: that would
# override the vendored ral-ripgrep-core's `all = "allow"` opt-out.  Every step
# carries the same value — one that differs re-fingerprints every unit in the
# graph, third-party crates included (measured 2026-08-25: 408 rebuilt).
RUSTFLAGS='-D warnings'
export RUSTFLAGS

# synod's tauri dependency links the GTK/WebKit stack, which no Linux host here
# carries; synod ships on macOS and Windows only.  Spliced unquoted below: a
# quoted empty $GUI would hand cargo an empty argument, which it rejects.
if [ "$MODE" = linux-box ] || [ "$(uname -s)" = Linux ]; then
    GUI='--exclude synod'
else
    GUI=''
fi

# Docker, not podman: docker is rootful, so --privileged is real host
# privilege.  Rootless podman's is namespace-root only, and if that falls short
# of bubblewrap's devpts mount the sandbox tests skip rather than fail — which
# reads exactly like a pass.
if [ "$MODE" = linux-box ]; then
    command -v docker >/dev/null 2>&1 || {
        echo 'ci.sh: linux-box needs docker on PATH' >&2
        exit 1
    }
    docker image inspect ral-linux-box >/dev/null 2>&1 || {
        echo 'ci.sh: image ral-linux-box is absent — run `just linux-box`' >&2
        exit 1
    }
fi

# Run one command where $MODE's toolchain lives.
#
# --privileged is load-bearing: bubblewrap mounts devpts for its virtual /dev,
# which an unprivileged container refuses, and every sandbox test would then
# skip instead of confining anything.  --init reaps the orphaned grandchildren
# a killed sandboxed process tree leaves behind, which otherwise read as "the
# timeout did not kill it".  The named volumes keep the registry and the Linux
# artefacts inside the VM instead of crossing the bind mount into the host's
# own target/.  RUSTUP_HOME is a volume too, or each step re-downloads the
# whole toolchain: rust-toolchain.toml tracks `stable` plus three extra
# targets, so every container start otherwise re-syncs the channel and throws
# it away on --rm.
at_toolchain() {
    case $MODE in
    native) "$@" ;;
    linux-box)
        docker run --rm --privileged --init --env RUSTFLAGS \
            -v "$PWD:/workspace" \
            -v ral-linux-cargo:/home/dev/.cargo \
            -v ral-linux-rustup:/usr/local/rustup \
            -v ral-linux-target:/workspace/.target-linux \
            ral-linux-box "$@"
        ;;
    esac
}

# Every cargo invocation goes through here, so each one is named once and the
# banner cannot drift from what runs.
cargo_step() {
    printf '\n\033[36m==>\033[0m \033[33mcargo %s\033[0m\n' "$*"
    at_toolchain cargo "$@"
}

banner() {
    printf '\n\033[36m==>\033[0m \033[33m%s\033[0m\n' "$1"
}

# /workspace/.target-linux is not in the image, so docker creates its volume
# root-owned.  Hand it to the image's baked-in `dev`, which has a real
# passwd/HOME entry — unlike an arbitrary `--user uid:gid`, for which
# getpwuid-based code (`whoami`, ral's own ~-expansion) misresolves or fails.
if [ "$MODE" = linux-box ]; then
    banner 'chown the Linux target volume'
    docker run --rm --user root -v ral-linux-target:/workspace/.target-linux \
        ral-linux-box chown dev:dev /workspace/.target-linux
fi

# Clippy's I/O-door denylist (clippy.toml) needs no `-D` flag: the
# `[workspace.lints.clippy] disallowed_methods = "deny"` table already makes a
# stray fs/process constructor a build break.
cargo_step clippy --workspace $GUI --all-targets

# Load-bearing before the tests: ral/tests/ invokes the `ral` binary as a
# subprocess, and `cargo test` alone does not reliably refresh it.
cargo_step build --workspace $GUI --all-targets
cargo_step test --workspace $GUI --features ral-core/test-util,exarch/test-util

# The Windows ABI cross-check never links, so a Unix host can run it.  exarch
# and synod are excluded because rustls -> aws-lc-sys compiles C against
# Windows system headers; guest-net because it depends on exarch, which brings
# that tree (fff-search -> git2 -> libgit2-sys) straight back.  The poisoned CC
# makes blake3's build script fall back to its pure-Rust intrinsics instead of
# hunting for ml64.exe.  Real Windows *test execution* still needs
# .github/workflows/windows.yml.  Skipped in linux-box mode: no rustup target
# in the image.
if [ "$MODE" = native ]; then
    CC_x86_64_pc_windows_msvc='cc-absent-use-blake3-pure-fallback'
    export CC_x86_64_pc_windows_msvc
    cargo_step check --workspace --exclude exarch --exclude synod \
        --exclude guest-net --all-targets --target x86_64-pc-windows-msvc
fi

# Always this host's python: the box's image carries neither uv nor the
# tree-sitter CLI, and render-site.py has no highlighting fallback.
banner 'uv run scripts/render-site.py'
uv run scripts/render-site.py

# plugins/*.ral are left out: the `_ed-*` builtins they call live only on the
# interactive shell's table, so a batch --check cannot type them — the REPL
# checks each plugin as rc loads it.  One `sh -ec` so the container starts once
# for all hundred files rather than once each.
banner 'ral --check examples/*/*.ral'
at_toolchain sh -ec 'for f in examples/*/*.ral; do cargo run -p ral --quiet -- --check "$f"; done'

printf '\n\033[32m  CI OK (%s).\033[0m\n' "$MODE"
