# justfile — a registry of the common dev commands for ral.
#
# This is an entry point, not a place for logic: every recipe is a
# single delegation to a `cargo` invocation or to one of the `.ral`
# scripts under scripts/, where the real orchestration lives.  The
# golden rule (AGENTS.md) applies here too — no shell branching or
# loops belong in this file; if a recipe wants them, it grows into a
# `.ral` script and this file calls that instead.
#
# The `.ral` scripts are launched through `cargo run -p ral`, never a
# bare `ral`: a fresh from-source checkout has no installed `ral` on
# PATH yet, so `just install` and friends must build the interpreter
# from source rather than presuppose it.  This also means every recipe
# exercises the current tree, not a stale installed binary.
#
# Run `just` with no arguments to list recipes.

# just runs every recipe through `sh -cu`, and a Windows box has no `sh`
# — without this, no recipe here runs there at all, not even `build`.
# PowerShell is the one shell Windows always has (`powershell.exe`, not
# `pwsh`, which ships separately).  This costs nothing on Unix, where the
# setting is ignored, and the recipes that matter on Windows are single
# `cargo` invocations that read identically under either shell.  The ones
# that are not — `check-windows`, with its POSIX `VAR=value cmd` prefix, the
# `.ral` script delegations, and the two `guest-*` image builds, which want
# bash and podman on a Unix host — are Unix-host recipes by design.  Where a
# recipe does need an environment variable set, the parameter carries it
# (`$ARCH` below): just exports it itself, so no shell-specific prefix
# appears in the recipe body at all.
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

# Cross-check the workspace against the shipping Windows ABI — catches
# `cfg(windows)` drift (unused imports, dead fields) a native check
# can't see.  exarch is excluded: its rustls -> aws-lc-sys dependency
# compiles C against Windows system headers, impossible from a Unix
# host, so exarch's Windows drift is gated by windows-check CI instead.
# synod sits atop exarch and inherits the same wall.
check-windows:
    CC_x86_64_pc_windows_msvc=cc-absent-use-blake3-pure-fallback RUSTFLAGS='-D warnings' cargo check --workspace --exclude exarch --exclude synod --all-targets --target x86_64-pc-windows-msvc

# Run the workspace test suite.  The `ral-core/test-util` feature pulls
# in the sandbox integration test that needs the confinement-token seam.
test:
    cargo test --workspace --features ral-core/test-util

# Format every crate in place.
fmt:
    cargo fmt

# Clippy across the workspace (the Cargo.toml workspace lints fire here, not
# under build).  `[workspace.lints.clippy] disallowed_methods = "deny"` makes
# the fs/process I/O-door denylist (clippy.toml) a hard error here, so a stray
# open/spawn outside a known door breaks the lint — no `-- -D` flag is needed,
# and using one would wrongly override the vendored ral-ripgrep-core's
# deliberate `[lints.clippy] all = "allow"` opt-out.  CI runs this exact
# command (.github/workflows/ci.yml).
lint:
    cargo clippy --workspace --all-targets

# Replay the Linux CI job locally (mirrors .github/workflows/ci.yml).
ci:
    cargo run -p ral --quiet -- scripts/ci-local.ral

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

# The guest boot media — Ubuntu's kernel plus this repo's initramfs, with
# ral-daemon and the engine inside it — into vm-image/out/boot/.  The one
# argument is a Debian architecture name, and everything else about the guest
# follows from it: mirror, musl target, the boot format the hypervisor's
# loader accepts, the driver set (SYNOD.md §2).  A Unix-host recipe: the
# script drives podman and refuses a container that is not the named
# architecture, because the media is built on the host that will boot it — so
# a Windows developer builds from WSL2 and finds the result in this same
# checkout.
# Boot media for one guest: no argument for the Mac's, `amd64` for Windows'.
guest-boot $ARCH='arm64':
    bash vm-image/build-boot.sh

# The guest rootfs — the pinned Ubuntu office userland of SYNOD.md §7 — into
# vm-image/out/.  Same architecture argument and the same Unix-host caveat as
# `guest-boot`, plus its own price: minutes of apt and a multi-GB tree that
# lives only inside the container.  The product is a raw ext4 image on both
# architectures; the fixed-VHD wrapper Hyper-V wants is added by the Windows
# backend at first boot, never here.
# Office userland for one guest: no argument for the Mac's, `amd64` for Windows'.
guest-rootfs $ARCH='arm64':
    bash vm-image/build.sh

# Boot the Windows machine layer once and hold it open, so a person can read
# the guest's own console lines and judge the boot honestly (`vm-manager`'s
# `boot-smoke` example).  Defaults to the media `guest-boot`/`guest-rootfs`
# leave in vm-image/out/, granting this checkout as the folder.
#
# Windows asks one thing of you first, and it is not a build step: the compute
# service serves only administrators and members of the local
# `Hyper-V Administrators` group, which is empty until somebody fills it.  So
# run this from an elevated terminal, or add the account once and sign out and
# in again:
#
#     net localgroup "Hyper-V Administrators" "%USERNAME%" /add
#
# Refused either way, the backend names that group rather than failing
# obscurely — which is itself worth seeing once.
[windows]
smoke-boot KERNEL='vm-image/out/boot/kernel' INITRAMFS='vm-image/out/boot/initramfs.img' ROOTFS='vm-image/out/rootfs.img' FOLDER='.':
    cargo run -p vm-manager --example boot-smoke -- {{KERNEL}} {{INITRAMFS}} {{ROOTFS}} {{FOLDER}}

# The same boot, and then one real run across the wire into the guest and back
# (`synod`'s `boot-run` example): boot, workspace share, engine, and the §3
# frame protocol under a running run, end to end.  Same prerequisite as
# `smoke-boot`, and one more of its own — this reads a file back out of the
# granted folder from inside the guest, so it needs the real rootfs rather than
# any bootable pair.
[windows]
smoke-run KERNEL='vm-image/out/boot/kernel' INITRAMFS='vm-image/out/boot/initramfs.img' ROOTFS='vm-image/out/rootfs.img' FOLDER='.':
    cargo run -p synod --example boot-run -- {{KERNEL}} {{INITRAMFS}} {{ROOTFS}} {{FOLDER}}

# Install the machine broker as a real Windows service, built from this
# checkout, so the privileged half of synod can be developed and watched
# without producing an MSI first.  It is the answer to the prerequisite the two
# `smoke-*` recipes above document: rather than every secretary joining
# `Hyper-V Administrators` — a group that may attach a physical disk to a
# machine and so read straight past NTFS — one `LocalSystem` service holds that
# privilege and creates machines on request over a named pipe.
# `synod/wix/broker-service.wxs` argues this at length and is what the MSI
# installs; these two recipes create the same service by hand, with the same
# name, account, and automatic start.  The one difference is that the MSI ships
# a copy of the binary and `sc.exe` merely records a path, so this service
# points into `target/release/` and every later `cargo build` quietly becomes
# what it starts next time.
#
# Both recipes need an elevated shell.  Creating or deleting a service is a
# privileged act by design, and from an ordinary terminal `sc.exe` will refuse
# with access denied and nothing more helpful.  That elevation is the whole of
# what synod ever asks for: the window that talks to this service over
# `\\.\pipe\synod-machine-broker` runs as nobody in particular, which is the
# entire reason the service exists.
#
# `broker-uninstall` takes it away again.  Its stop is allowed to fail (the `-`
# prefix), because a service that is not running is already the state the
# delete on the next line wants.
# The machine broker as a live LocalSystem service, from this checkout: needs an elevated shell.
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

# Bundle synod into a runnable macOS .app (+ .dmg) under
# target/release/bundle/.  Two prerequisites this recipe does not install
# for you: the Tauri CLI (`cargo install tauri-cli`) and a built arm64 guest
# image under `vm-image/out/` (`just guest-rootfs` then `just guest-boot`) —
# the bundle embeds that image's kernel, initramfs, and compressed rootfs.
# Ad-hoc signing (tauri.conf.json `signingIdentity` "-") carries the
# virtualization entitlement, so the .app creates VMs locally with no
# Apple Developer account; distributing it to other Macs additionally
# needs a Developer ID signature and notarization.
[working-directory('synod')]
synod-app:
    cargo tauri build

# Bundle synod into a Windows installer (.msi) under
# target/release/bundle/msi/.  Two prerequisites this recipe does not build
# for you: the Tauri CLI (`cargo install tauri-cli`), and an **x86_64** guest
# image under `vm-image/out/` — `just guest-rootfs amd64` then
# `just guest-boot amd64`, run from a Unix host, as those recipes explain.
# The WiX toolset the CLI drives is not a prerequisite: it fetches that
# itself on first use.
#
# Like `synod-app`, this bundle ships the guest image — but flat, not as the
# `.app`'s zstd archive and checksum: `tauri.windows.conf.json` maps three
# resources into `boot/`, with the rootfs the raw image it was built as.  An
# MSI compresses its own cabinet, so the archive would only be compressed
# twice; and the alternative — inflating at first launch, the way the `.app`
# must — writes two gigabytes into one user's cache, which is no use to a
# `LocalSystem` service that has no user and no decompressor.  It ships them
# because there is now
# something to boot them with — a Hyper-V machine created through the Host
# Compute System API (SYNOD.md §2), taking the kernel directly as a bzImage,
# carrying the control plane over Hyper-V sockets into the guest's own
# `AF_VSOCK`, and serving the granted folder as a 9p share over a vsock port.
# The images ship as the raw ext4 they are built as; the fixed-VHD wrapper
# Hyper-V insists on is applied by that backend at first boot.
#
# Unlike the .app, this installer ships two executables and registers one
# service.  `synod/wix/broker-service.wxs` — reached through
# `tauri.windows.conf.json`'s `fragmentPaths` — adds the machine broker beside
# synod in the installation directory and declares it as `SynodMachineBroker`,
# a `LocalSystem` service started on install and removed on uninstall, so that
# no user of synod has to be given the privilege a virtual machine requires.
# WiX only *references* that binary, which is why the release build of it is the
# first line below rather than a prerequisite left to the reader: the reference
# is a path into this build's profile directory, and a missing file there
# surfaces as a linker error from WiX with no hint of what to do about it.  The
# guest image is not duplicated for the broker — it reads `boot/` beside its own
# executable, which is the same folder synod's copy already lands in.
#
# What the installer delivers is therefore the whole of synod on Windows: the
# grant, the prompt, the folder's before/after checkpoint, the change report,
# the undo — and a machine to do the work inside.  It needs Hyper-V available
# on the machine, which means Windows Pro, Education, or Enterprise (the
# university fleet's editions); Home has no Hyper-V and is unsupported (§2).
[working-directory('synod')]
synod-msi:
    cargo build --release -p vm-manager --bin synod-machine-broker
    cargo tauri build
