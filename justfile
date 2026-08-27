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

# Private, and a dependency of `test`: `cargo test` alone does not reliably
# refresh the `ral` binary that ral/tests/ shells out to.
_build $RUSTFLAGS=deny:
    cargo build --workspace {{gui}} --all-targets

# Run the workspace test suite.
test $RUSTFLAGS=deny: _build
    cargo test --workspace {{gui}} --features ral-core/test-util,exarch/test-util

# No `-- -D warnings`: it'd override vendored ral-ripgrep-core's `all = "allow"`
# opt-out. Pedantry (pedantic + nursery, I/O-door denylist) lives in RUSTFLAGS
# and `[workspace.lints.clippy]` instead.

# Clippy across the workspace, warnings as errors.
lint $RUSTFLAGS=deny:
    cargo clippy --workspace {{gui}} --all-targets

# Never links, so a Unix host can run it. exarch and synod are excluded because
# rustls -> aws-lc-sys compiles C against Windows system headers; guest-net
# because it depends on exarch, which brings that tree (fff-search -> git2 ->
# libgit2-sys) straight back. The poisoned CC makes blake3's build script fall
# back to its pure-Rust intrinsics instead of hunting for ml64.exe. Real
# Windows *test execution* still needs .github/workflows/windows.yml.

# Cross-check the workspace against the shipping Windows ABI.
check-windows $RUSTFLAGS=deny $CC_x86_64_pc_windows_msvc='cc-absent-use-blake3-pure-fallback':
    cargo check --workspace --exclude exarch --exclude synod --exclude guest-net --all-targets --target x86_64-pc-windows-msvc

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
