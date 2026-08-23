# justfile — a registry of the common dev commands for ral.
#
# This is an entry point, not a place for logic: every recipe is a
# single delegation to a `cargo` invocation or to one of the `.ral`
# scripts under scripts/, where the real orchestration lives.
#
# Run `just` with no arguments to list recipes.

# Windows has no `sh`; PowerShell is the shell it always has.
set windows-shell := ['powershell.exe', '-NoLogo', '-NoProfile', '-Command']

# synod ships on macOS and Windows only, and its tauri dependency links GTK,
# which a Linux host is not expected to carry.  CI's Linux job excludes it for
# exactly this reason; this is the same exclusion, written once so that no
# recipe below hand-rolls it and no Linux developer has to.
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

# Builds first because the integration tests in ral/tests/ invoke the `ral`
# binary as a subprocess, and `cargo test` alone does not reliably refresh it.

# Run the workspace test suite.
test: build
    cargo test --workspace {{gui}} --features ral-core/test-util,exarch/test-util

# Format every crate in place.
fmt:
    cargo fmt

# No `-- -D warnings`: that would override the vendored ral-ripgrep-core's
# `all = "allow"` opt-out.  The pedantry comes from RUSTFLAGS plus the
# `[workspace.lints.clippy]` table (pedantic + nursery, deny on the I/O-door
# denylist), which is where a lint decision belongs.

# Clippy across the workspace, warnings as errors — exactly what CI lints with.
lint $RUSTFLAGS='-D warnings':
    cargo clippy --workspace {{gui}} --all-targets

# Lint and test the workspace: the gate before every commit.
gate: lint test

# Replay CI on this host: the gate plus the build, the Windows cross-check, the site, and the examples.
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
