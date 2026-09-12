# justfile — a registry of dev commands for ral, not a place for logic:
# every recipe delegates to `cargo`, to scripts/ci.sh, or to a `.ral` script
# under scripts/.  Run `just` with no arguments to list recipes.

# Windows has no `sh`; PowerShell is the shell it always has.
set windows-shell := ['powershell.exe', '-NoLogo', '-NoProfile', '-Command']

# synod's tauri dependency links GTK, absent on Linux; excluded there, once.
gui := if os() == "linux" { "--exclude synod" } else { "" }

# Show the recipe list.
default:
    @just --list

# Warnings are errors in every recipe scripts/ci.sh runs, and all of them carry
# this one value: a step differing re-fingerprints every unit in the graph,
# third-party crates included (measured 2026-08-25: 408 rebuilt).
deny := '-D warnings'

# `--keep-going`, here and on `lint`: a catch-up session walks into a pile of
# drift at once — a cross-platform half nothing has compiled in weeks — and
# stopping at the first crate turns one pass into one fix-and-rebuild cycle per
# failure. Exit status is unchanged; only how much of the pile one run reports.

# Private, and a dependency of `test`: `cargo test` alone does not reliably
# refresh the `ral` binary that ral/tests/ shells out to.
_build $RUSTFLAGS=deny:
    cargo build --workspace {{gui}} --all-targets --keep-going

# Run the workspace test suite.
test $RUSTFLAGS=deny: _build
    cargo test --workspace {{gui}} --features ral-core/test-util,exarch/test-util

# Compiles nothing, so it is the cheapest step CI has and the one worth failing
# first. `--all` reaches vendored ral-ripgrep-core too — unlike the clippy
# opt-out, upstream is already rustfmt-clean, so nothing there is reformatted.

# Check the whole workspace is rustfmt-clean.
fmt-check:
    cargo fmt --all --check

# No `-- -D warnings`: it'd override vendored ral-ripgrep-core's `all = "allow"`
# opt-out. Pedantry (pedantic + nursery, I/O-door denylist) lives in RUSTFLAGS
# and `[workspace.lints.clippy]` instead.

# Clippy across the workspace, warnings as errors.
lint $RUSTFLAGS=deny:
    cargo clippy --workspace {{gui}} --all-targets --keep-going

# Never links, so a Unix host can run it. `cargo xwin` (rust-cross/cargo-xwin)
# splats the real MSVC CRT and Windows SDK headers exarch's `rustls ->
# aws-lc-sys` and `guest-net`'s `fff-search -> git2 -> libgit2-sys` need to
# actually compile C — no workspace member is excluded. One-time host setup:
#   brew install llvm lld && cargo install cargo-xwin
# then put both kegs' bin/ on PATH (they're keg-only). Real Windows *test
# execution* still needs .github/workflows/windows.yml.

# Cross-check the workspace against the shipping Windows ABI.
check-windows $RUSTFLAGS=deny:
    cargo xwin check --workspace --all-targets --target x86_64-pc-windows-msvc

# Cross-check the Linux sandbox, which a macOS `lint` never compiles.
#
# Both arches because the seccomp filter is arch-conditional, and its x32 guard
# is x86-64 only: one arch proves half of it. Both are rust-toolchain.toml's,
# so no host needs a target it was not already going to install. Scoped to
# ral-core: ral-ripgrep-core's tikv-jemallocator compiles jemalloc's C, which
# needs a musl cross toolchain ("C compiler cannot create executables").
check-linux $RUSTFLAGS=deny:
    cargo check -p ral-core --all-targets --target aarch64-unknown-linux-musl
    cargo check -p ral-core --all-targets --target x86_64-unknown-linux-musl

# Cross-check the macOS half, which a Linux `lint` never compiles — check-linux
# read the other way round, and the gap a `cfg(unix)` helper falls through when
# its only caller is `cfg(target_os = "linux")`: live on a Linux host, dead code
# on a Mac, and a build break there alone.
#
# One arch, Seatbelt being no more arch-conditional than the rest of it. Never
# links, so a Linux host can run it, and the bogus CC sends blake3's build
# script down its pure-Rust path as check-windows does. Unlike check-linux's,
# this target is one rust-toolchain.toml installs for this recipe's sake alone.
check-macos $RUSTFLAGS=deny $CC_aarch64_apple_darwin='cc-absent-use-blake3-pure-fallback':
    cargo check -p ral-core --all-targets --target aarch64-apple-darwin

# plugins/*.ral are left out: the `_ed-*` builtins they call live only on the
# interactive shell's table, so a batch --check cannot type them — the REPL
# checks each plugin as rc loads it. One shell for all hundred files: under
# `just linux-ci` the container then starts once, not once per file.

# Type-check every example under examples/.
[unix]
examples-check:
    #!/bin/sh
    set -eu
    for f in examples/*/*.ral; do cargo run -p ral --quiet -- --check "$f"; done

# Drives the REPL load path examples-check can't reach: an rc naming exactly
# one plugin, a fresh XDG_CONFIG_HOME per plugin so none can mask another's
# failure, RAL_PATH pointed at this checkout's plugins/. A failing plugin
# prints `ral: plugin 'NAME': ...` on stderr and the REPL carries on regardless
# (exit 0), so that line — not the exit status — is the failure signal.
[unix]
plugins-check:
    #!/bin/sh
    set -eu
    fail=0
    for f in plugins/*.ral; do
        name=$(basename "$f" .ral)
        dir=$(mktemp -d)
        mkdir -p "$dir/ral"
        printf "return [plugins: [[plugin: '%s']]]" "$name" > "$dir/ral/rc"
        out=$(echo 'echo ok' | XDG_CONFIG_HOME="$dir" RAL_PATH="{{justfile_directory()}}/plugins" cargo run -p ral --quiet -- -i 2>&1)
        if echo "$out" | grep -q "ral: plugin '$name':"; then
            echo "plugins-check: $name failed to load"
            echo "$out" | grep "ral: plugin '$name':"
            fail=1
        fi
    done
    exit $fail

# The one check that reads both of the window's languages at once: `ts-rs`
# writes the TypeScript for every type crossing the Tauri seam, and `deno
# check` reads synod/ui/js against it — so a `case` the Rust has no variant
# for is a build failure rather than a renderer nothing ever reaches.
#
# Where the types land is each type's own `export_to`, not this recipe's: a
# bare `cargo test` runs the same export, and a destination that depended on
# the invocation would strew a second, stale copy through the tree. They are
# generated and never committed — one copy of the fact, no staleness to police.
#
# --unstable-sloppy-imports because ts-rs emits extensionless relative imports
# between the types it generates, which Deno otherwise refuses.
#
# Skipped on Linux for the reason `gui` excludes synod there: generating the
# types means compiling synod, and its tauri dependency links GTK.
[unix]
ui-check:
    #!/bin/sh
    set -eu
    if [ "{{ os() }}" = linux ]; then
        echo 'ui-check: skipped — synod does not build on Linux'
        exit 0
    fi
    cargo test -p synod --quiet export_bindings
    cd synod/ui && deno check --unstable-sloppy-imports js/*.js

# Both run scripts/ci.sh — the same step list GitHub Actions runs, differing
# only in where cargo runs.  It calls the recipes above rather than spelling
# out their cargo lines, so `just test` is CI's test and cannot drift from it;
# it stays POSIX sh so CI still runs when ral does not build.

# Run CI on this host's toolchain: lint, build, test, the Windows cross-check, the site, the examples.
[unix]
ci:
    scripts/ci.sh native

# Run that same CI inside the Linux container — the only place a macOS host compiles the bwrap sandbox.
[unix]
linux-ci:
    scripts/ci.sh linux-box

# Build the container `linux-ci` runs in.
linux-box:
    docker build -t ral-linux-box -f scripts/linux-box.Dockerfile scripts

# Render the static site into site/.
site:
    uv run scripts/render-site.py

# Build and install ral, exarch, and ral-sh from source.
install:
    cargo run -p ral --quiet -- scripts/install.ral

# Build the release matrix, passing flags through, e.g. `just release --local`.
release *args:
    cargo run -p ral --quiet -- scripts/build-release.ral {{args}}

# Build release and time the bench/*.ral benchmarks with hyperfine (dev/docs/plans/260825_cek_machine.md §6.3).
bench:
    cargo run -p ral --quiet -- scripts/bench.ral

# Run the current source as `ral`, forwarding arguments, e.g. `just run examples/hello.ral`.
run *args:
    cargo run -p ral --quiet -- {{args}}

# Boot media for one guest: no argument for the Mac's, `amd64` for Windows'.
guest-boot $ARCH='arm64':
    bash vm-image/build-boot.sh

# Office userland for one guest: no argument for the Mac's, `amd64` for Windows'.
guest-rootfs $ARCH='arm64':
    bash vm-image/build.sh

# Boot the Windows machine layer and hold it open (needs an elevated shell / Hyper-V Administrators).
[windows]
smoke-boot KERNEL='vm-image/out/boot/kernel' INITRAMFS='vm-image/out/boot/initramfs.img' ROOTFS='vm-image/out/rootfs.img' FOLDER='.':
    cargo run -p vm-manager --example boot-smoke -- {{KERNEL}} {{INITRAMFS}} {{ROOTFS}} {{FOLDER}}

# The same boot, plus one real run into the guest and back (same prerequisite as smoke-boot).
[windows]
smoke-run KERNEL='vm-image/out/boot/kernel' INITRAMFS='vm-image/out/boot/initramfs.img' ROOTFS='vm-image/out/rootfs.img' FOLDER='.':
    cargo run -p synod --example boot-run -- {{KERNEL}} {{INITRAMFS}} {{ROOTFS}} {{FOLDER}}

# Install the machine broker as a LocalSystem Windows service (needs an elevated shell).
[windows]
broker-install:
    cargo build --release -p vm-manager --bin synod-machine-broker
    sc.exe create SynodMachineBroker binPath= "{{justfile_directory()}}\target\release\synod-machine-broker.exe" DisplayName= "Synod machine broker" type= own start= auto obj= LocalSystem
    sc.exe description SynodMachineBroker "Creates and stops the virtual machines Synod runs its work in. Synod itself runs without privileges; this service holds the one privilege the platform requires for a virtual machine, and nothing else."
    sc.exe start SynodMachineBroker

# Stop that service and take it off this computer again: elevated shell too.
[windows]
broker-uninstall:
    -sc.exe stop SynodMachineBroker
    sc.exe delete SynodMachineBroker

# Bundle synod into a macOS .app (+ .dmg); needs the Tauri CLI and a built arm64 guest image.
[working-directory('synod')]
synod-app:
    cargo tauri build

# Bundle synod into a Windows .msi installer; needs the Tauri CLI and an x86_64 guest image (built from a Unix host).
[working-directory('synod')]
synod-msi:
    cargo build --release -p vm-manager --bin synod-machine-broker
    cargo tauri build
