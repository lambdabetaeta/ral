# justfile — a registry of the common dev commands for ral.
#
# This is an entry point, not a place for logic: every recipe is a
# single delegation to a `cargo` invocation or to one of the `.ral`
# scripts under scripts/, where the real orchestration lives.
#
# Run `just` with no arguments to list recipes.

# Windows has no `sh`; PowerShell is the shell it always has.
set windows-shell := ['powershell.exe', '-NoLogo', '-NoProfile', '-Command']

# Show the recipe list.
default:
    @just --list

# Build the whole workspace, including tests and examples.
build:
    cargo build --workspace --all-targets

# Type-check the workspace without producing binaries — the fast dev loop.
check:
    cargo check --workspace --all-targets

# Cross-check the workspace against the shipping Windows ABI (exarch/synod/guest-net excluded: their C deps can't cross-compile from Unix).
check-windows:
    CC_x86_64_pc_windows_msvc=cc-absent-use-blake3-pure-fallback RUSTFLAGS='-D warnings' cargo check --workspace --exclude exarch --exclude synod --exclude guest-net --all-targets --target x86_64-pc-windows-msvc

# Run the workspace test suite.
test:
    cargo test --workspace --features ral-core/test-util

# Format every crate in place.
fmt:
    cargo fmt

# Clippy across the workspace.
lint:
    cargo clippy --workspace --all-targets

# Replay CI on this host.
ci:
    cargo run -p ral --quiet -- scripts/ci-local.ral

# Replay the Linux half of CI in a container.
linux-ci:
    cargo run -p ral --quiet -- scripts/linux-ci.ral

# Build the container `linux-ci` runs in.
linux-box:
    podman build -t ral-linux-box -f scripts/linux-box.Dockerfile scripts

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
