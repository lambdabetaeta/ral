set windows-shell := ['powershell.exe', '-NoLogo', '-NoProfile', '-Command']

mod vm 'vm-image/mod.just'
mod synod 'synod/mod.just'

# Warnings fail every recipe, rustdoc and clippy included.  Cargo denies them
# after compiling, so unlike `RUSTFLAGS=-D warnings` this leaves fingerprints
# alone: a bare `cargo` call reuses the artifacts a recipe built, and the
# rustflags in a machine's cargo config are not overridden.
export CARGO_BUILD_WARNINGS := env('CARGO_BUILD_WARNINGS', 'deny')

# What releases ship, so CI compiles what ships.
features := 'ral/allutils'

# Where `cargo install` puts binaries, and `install` the notices beside them.
cargo_home := env('CARGO_HOME', home_directory() / '.cargo')

[private]
default:
    @just --list --list-submodules

# Build all workspace targets.
build:
    cargo build --workspace --all-targets --features {{features}} --keep-going

# Run runtime tests, optionally only those whose path matches, e.g. `just test corpus::`.
test *filter: build
    cargo test --workspace --features {{features}},ral-core/test-util,exarch/test-util --no-fail-fast -- {{filter}}

# Run unit tests alone: the fast inner loop.
test-unit:
    cargo test --workspace --lib --bins --features {{features}},ral-core/test-util,exarch/test-util

# Rewrite the run corpus's goldens (tests/run/**/*.out) from current output.
bless $RAL_BLESS='1':
    cargo test -p ral --features {{features}} --test integration -- corpus::run

# Check the authority API's compile-time guarantees (separate compiler invocations).
test-types:
    cargo test --workspace --test markers --features {{features}},ral-core/test-util,exarch/test-util -- --ignored

# Rustfmt, clippy, rustdoc's link checks, and the source's structural laws.
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --features {{features}} --keep-going
    cargo doc --workspace --no-deps --document-private-items --features {{features}}
    cargo test -p ral-lint

# Clippy for the platforms an x86-64 host does not build: x86-64 musl and Windows (needs clang, lld, cargo-xwin).
check-x86 $CC_x86_64_unknown_linux_musl='clang':
    rustup -q target add x86_64-unknown-linux-musl x86_64-pc-windows-msvc
    cargo clippy -p ral-core -p ral --all-targets --features {{features}} --target x86_64-unknown-linux-musl
    cargo xwin clippy --workspace --all-targets --features {{features}} --target x86_64-pc-windows-msvc --keep-going

# Clippy for the platforms an arm64 host does not build: arm64 musl and macOS (needs clang, lld; macOS needs a Mac for its C sysroot).
check-arm64 $CC_aarch64_unknown_linux_musl='clang' $CC_aarch64_apple_darwin='clang':
    rustup -q target add aarch64-unknown-linux-musl aarch64-apple-darwin
    cargo clippy -p ral-core -p ral --all-targets --features {{features}} --target aarch64-unknown-linux-musl
    cargo clippy -p ral-core -p ral -p vm-manager --all-targets --features {{features}} --target aarch64-apple-darwin

# Build the site and the wiki into site/.
docs: site wiki

# ts-rs emits extensionless imports, hence sloppy imports.

# Type-check synod's renderer against the TypeScript its Rust types export.
[working-directory('synod/ui')]
ui: _bindings
    deno check --unstable-sloppy-imports js/*.js
    deno test --unstable-sloppy-imports tests/

# Test the editor grammars against their fixtures (needs tree-sitter and node).
[unix]
editors:
    cd editors/tree-sitter-ral && tree-sitter generate && tree-sitter test
    bash editors/ral-syntax/tests/run.sh

_bindings:
    cargo test -p synod --quiet export_bindings

# Render the static site into site/.
[private]
site:
    uv run scripts/render-site.py

# Build the wiki into site/wiki/ with Quartz; `just wiki --serve` previews it.
[private]
wiki *args:
    bash scripts/wiki/build.sh {{args}}

# Everything GitHub CI runs that this host can.
ci: lint test test-types docs ui

# Run recipes in the Linux container, e.g. `just box lint test`.
[unix]
box +recipes:
    #!/bin/sh
    set -eu
    engine=$(command -v docker || command -v podman) || { echo 'box: needs docker or podman on PATH' >&2; exit 1; }
    "$engine" build -q -t ral-linux-box -f scripts/linux-box.Dockerfile scripts >/dev/null
    # A fresh named volume is root-owned.
    "$engine" run --rm --user root -v ral-linux-target:/workspace/.target-linux ral-linux-box chown dev:dev /workspace/.target-linux
    # --privileged: bwrap needs an unmasked /proc and a fresh devpts.  --init
    # reaps orphaned sandbox trees.  A tmpfs /tmp: fuse-overlayfs answers
    # ENOENT for ENAMETOOLONG.
    exec "$engine" run --rm --privileged --init -e RAL_REQUIRE_SANDBOX=1 -e CARGO_BUILD_JOBS \
        --tmpfs /tmp:rw,exec,mode=1777 \
        -v "$PWD:/workspace" \
        -v ral-linux-cargo:/home/dev/.cargo \
        -v ral-linux-rustup:/usr/local/rustup \
        -v ral-linux-target:/workspace/.target-linux \
        ral-linux-box sh -c 'rustup -q target add "$(uname -m)-unknown-linux-musl" && exec just "$@"' box {{recipes}}

# Build and install ral and exarch from source, with their notices.
[unix]
install: _install-crates
    #!/bin/sh
    set -eu
    cargo_home={{quote(cargo_home)}}
    for package in ral exarch; do
        mkdir -p "$cargo_home/share/$package"
        cp LICENSE-APACHE LICENSE-MIT NOTICE "$cargo_home/share/$package"
    done
    cp vendor/edtui/LICENSE.md "$cargo_home/share/exarch/LICENSE-EDTUI"
    # ral-sh is a name, not a binary: the POSIX bridge lives in ral and keys on argv[0].
    ln -sfn ral "$cargo_home/bin/ral-sh"
    # Ad-hoc signing is a macOS ritual.
    if [ "$(uname -s)" = Darwin ]; then codesign -s - "$cargo_home/bin/ral"; fi

# Build and install ral and exarch from source, with their notices.
[windows]
install: _install-crates
    New-Item -ItemType Directory -Force '{{cargo_home}}/share/ral', '{{cargo_home}}/share/exarch' > $null
    Copy-Item LICENSE-APACHE, LICENSE-MIT, NOTICE '{{cargo_home}}/share/ral'
    Copy-Item LICENSE-APACHE, LICENSE-MIT, NOTICE '{{cargo_home}}/share/exarch'
    Copy-Item vendor/edtui/LICENSE.md '{{cargo_home}}/share/exarch/LICENSE-EDTUI'

_install-crates:
    cargo +stable install --force --locked --path ral
    cargo +stable install --force --locked --path exarch

# Build the release matrix, passing flags through, e.g. `just release --local`.
release *args:
    cargo run -p ral --quiet -- scripts/build-release.ral {{args}}

# Build release and time the bench/*.ral benchmarks with hyperfine.
bench:
    cargo run -p ral --quiet -- scripts/bench.ral
