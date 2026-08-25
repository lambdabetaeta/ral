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

# Build the whole workspace, including tests and examples.
build:
    cargo build --workspace {{gui}} --all-targets

# Type-check the workspace without producing binaries — the fast dev loop.
check:
    cargo check --workspace {{gui}} --all-targets

# Cross-check the workspace against the shipping Windows ABI (exarch/synod/guest-net excluded: their C deps can't cross-compile from Unix).
check-windows $RUSTFLAGS='-D warnings' $CC_x86_64_pc_windows_msvc='cc-absent-use-blake3-pure-fallback':
    cargo check --workspace --exclude exarch --exclude synod --exclude guest-net --all-targets --target x86_64-pc-windows-msvc

# `linux-ci` covers this ground properly, in a container; this is seconds
# because `check` never links. `ral`/`ral-ripgrep-core` are excluded on top of
# the Windows twin's three: musl gives ripgrep a jemalloc allocator with no
# pure-Rust fallback, so the absent-CC trick that spares blake3 can't spare it.

# Cross-check the guest's musl target — the only cheap check of the `cfg(target_os = "linux")` code (vsock, hatch, spawn jail) a macOS host gates out entirely.
check-linux $RUSTFLAGS='-D warnings' $CC_x86_64_unknown_linux_musl='cc-absent-use-blake3-pure-fallback':
    cargo check --workspace --exclude exarch --exclude synod --exclude guest-net --exclude ral --exclude ral-ripgrep-core --all-targets --target x86_64-unknown-linux-musl

# Builds first: `cargo test` alone won't reliably refresh the `ral` binary
# that ral/tests/ shells out to.

# Run the workspace test suite.
test: build
    cargo test --workspace {{gui}} --features ral-core/test-util,exarch/test-util

# Format every crate in place.
fmt:
    cargo fmt

# No `-- -D warnings`: it'd override vendored ral-ripgrep-core's `all = "allow"`
# opt-out. Pedantry (pedantic + nursery, I/O-door denylist) lives in RUSTFLAGS
# and `[workspace.lints.clippy]` instead.

# Clippy across the workspace, warnings as errors — exactly what CI lints with.
lint $RUSTFLAGS='-D warnings':
    cargo clippy --workspace {{gui}} --all-targets

# Lint and test the workspace: the gate before every commit.
gate: lint test

# Both run scripts/ci.sh — the same step list GitHub Actions runs, differing
# only in where cargo runs.  POSIX sh, so `just ci` still works when ral does
# not build; that is the point of it not being a `.ral` script.

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
