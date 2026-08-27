#!/bin/sh
# ci.sh — the one CI step list.  GitHub Actions runs it (.github/workflows/ci.yml),
# `just ci` runs it before a push, `just linux-ci` runs it in the container.
#
# Every step is a justfile recipe, invoked by name.  The justfile is where a
# developer's commands already live, so this script owning only the *order* and
# the *place* leaves one definition of what linting and testing the workspace
# means — `just lint` locally is CI's lint, not a copy that resembles it.
#
# POSIX sh, and it may call `just` but never `ral`: ral is the thing under test,
# and a CI that cannot run when ral is broken cannot report that ral is broken.
# A justfile is an inert recipe registry that fails loudly on a parse error.
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

# RUSTFLAGS and synod's exclusion on Linux are the justfile's (`deny`, `gui`),
# not repeated here.  `gui` keys off `os()`, which — because the recipes run
# inside the container in linux-box mode — reports the platform cargo actually
# targets, so no second rule here has to predict it.

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
        docker run --rm --privileged --init \
            -v "$PWD:/workspace" \
            -v ral-linux-cargo:/home/dev/.cargo \
            -v ral-linux-rustup:/usr/local/rustup \
            -v ral-linux-target:/workspace/.target-linux \
            ral-linux-box "$@"
        ;;
    esac
}

banner() {
    printf '\n\033[36m==>\033[0m \033[33m%s\033[0m\n' "$1"
}

# Every step goes through here, so each is named once and the banner cannot
# drift from what runs.  `just` echoes the cargo line it expands to, so the
# recipe name and the command both appear.
step() {
    banner "just $*"
    at_toolchain just "$@"
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
step lint

# `test` depends on `_build`, which is load-bearing: ral/tests/ invokes the
# `ral` binary as a subprocess, and `cargo test` alone won't refresh it.
step test

# Skipped in linux-box mode: the image carries no rustup Windows target.  Run
# on the host either way — the recipe never links, so it costs seconds.
if [ "$MODE" = native ]; then
    banner 'just check-windows'
    just check-windows
fi

# Always this host's python: the box's image carries neither uv nor the
# tree-sitter CLI, and render-site.py has no highlighting fallback.
banner 'just site'
just site

step examples-check

printf '\n\033[32m  CI OK (%s).\033[0m\n' "$MODE"
