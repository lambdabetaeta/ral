# vm-image — the synod guest rootfs

This directory builds `rootfs.img`: the **read-only Ubuntu 26.04 LTS (resolute)
office userland** that synod boots inside its guest VM, for either of §2's two
guests — **arm64** for Virtualization.framework on macOS, **amd64** for Hyper-V
on Windows. It is one of the three disks in SYNOD.md §7's stack:

> §7 pins **26.04 LTS**, the current LTS — a question this pipeline settled by
> validating the package set identically against 24.04 and 26.04. The suite
> stays a one-line env knob (`SUITE`); the translation of §7's prose into real
> package names is under *Corrections* below.

```
session.img  (RW, per-session)   overlayfs upper: /work + guest scratch
rootfs.img   (RO, pinned)   <--   build.sh BUILDS THIS ONE
boot.img     (ships w/ app)      kernel + ral-daemon + ral engine (build-boot.sh)
```

## Why the image is what an agent may rely on

The guest reaches the network only through the host's allowlist (§6), and no
hypervisor here configures a network adapter: its only interface is a `tun`
whose single peer is `guest-net`, a user-mode TCP/IP stack in a host process
that answers DNS itself, terminates TCP, and intercepts 80 and 443 so a grant
is a *verb on a host* rather than a hostname. So installs at runtime are
possible — and they are also **forgotten at the next boot**, because the root
overlay is per-session (§7). That is what makes the rootfs load-bearing: not
that nothing can be installed, but that nothing installed *lasts*, so whatever
an agent may rely on is exactly what is in the image. Hence an image that is
deliberately rich — a full document stack, a wide font set, full locales — and
just as deliberately lean where it counts: **no compilers, headers, or build
systems**. Those earn their weight only for software that outlives the session,
and here nothing does.

No ral binaries live here either; the daemon and engine ship in `boot.img`,
versioned with the app so the `Attach` handshake can enforce the pairing (§7).

## Two guests, one recipe

§2 fixes the platform matrix: macOS runs Virtualization.framework on arm64,
Windows runs Hyper-V on x86_64. The *userland* does not care — an office
userland is an office userland — so both scripts here take a single `ARCH`
knob (`arm64`, the default, or `amd64`) and derive everything else from it:

| | `ARCH=arm64` | `ARCH=amd64` |
|---|---|---|
| Host that builds it | Apple-silicon Mac | x86_64 Linux, or WSL2 on the Windows box |
| Mirror | `ports.ubuntu.com/ubuntu-ports` | `archive.ubuntu.com/ubuntu` |
| Rust target (boot media) | `aarch64-unknown-linux-musl` | `x86_64-unknown-linux-musl` |
| Kernel handed to the loader | raw ARM64 `Image` | x86 `bzImage` |
| Guest device model | virtio | Hyper-V VMBus |

Two consequences worth stating before they surprise anyone:

- **The mirrors are not interchangeable.** arm64 packages live only on
  `ports.ubuntu.com`, amd64 packages only on `archive.ubuntu.com`. Both
  scripts derive the mirror from `ARCH`, so the pair cannot disagree, and an
  explicit `MIRROR` still wins (for a `snapshot.ubuntu.com` pin).
- **You build the media for the host that will boot it.** The container is the
  image's own architecture, and each script refuses a mismatch on its first
  step rather than fail obscurely later: apt resolves the kernel package for
  the container's native architecture, and the `fc-cache` pass and every
  spot-check *execute* guest binaries. Nothing here is cross-architecture, and
  no qemu is involved on either host.

Because a developer builds one guest at a time, `out/` deliberately does **not**
encode the architecture in any path — `synod/src/boot.rs` and the bundles'
resource maps look in the one fixed place. `out/boot/boot-manifest.txt`'s
leading `arch=` line is how you know which guest is currently sitting there.

## Contents

| File | What it is |
|------|-----------|
| `packages.txt` | The pinned package set, one per line, grouped to mirror §7's rationale. Comments and blanks are stripped by the build. |
| `build.sh` | The rootfs pipeline: one strict-mode script that runs a container and emits the image, checksum, manifest, and verification. |
| `build-boot.sh` | The boot-media pipeline: Ubuntu's kernel plus this repo's initramfs, with `ral-daemon` and the engine inside it (see the last section). |
| `out/` | Build products (git-ignored), for whichever architecture was built last. |

Build products in `out/`:

- `rootfs.img` — the ext4 image (what the hypervisor mounts)
- `rootfs.img.zst` — zstd-compressed image (what a user downloads)
- `rootfs.img.sha256`, `rootfs.img.zst.sha256` — checksums of both
- `packages-manifest.txt` — `dpkg -l` capture; the exact version-pin record
- `verify.txt` — spot-check results
- `build.log` — full build transcript

## How the pipeline works

Neither macOS nor Windows has `mmdebstrap` or ext4 tooling, so the build runs
entirely inside a **`ubuntu:24.04` container of the image's own architecture**.
That base image is multi-arch, so podman pulls the host's native build and no
qemu emulation is involved either way; the container asserts as much before it
starts (see *Two guests, one recipe*). The 24.04 base only supplies the build
*tooling* — `mmdebstrap` builds any suite, and its shipped keyring already
verifies 26.04's archive signature. Inside it:

1. **`mmdebstrap`** assembles the rootfs *tree* from `packages.txt`.
2. **`fc-cache -f`** in the tree pre-builds the fontconfig cache. Without it the
   first fontconfig consumer in the guest (LibreOffice, ImageMagick, any
   renderer) pays a cold scan over the whole wide font set on first boot,
   dominated by the Noto CJK faces and measured here at *minutes*. Baking
   `/var/cache/fontconfig` into the image makes first use fast.
3. **`mkfs.ext4 -d <tree>`** turns that directory straight into an ext4 image —
   **no loop mount, no privilege on the image step** — then **`resize2fs -M`**
   shrinks it to the smallest size the content needs. `rootfs.img` is the RO
   overlay *lower* layer (§7): immutable at the block layer, it never grows
   (all guest writes land in the overlay *upper* on `session.img`), so free
   space inside it would be pure download bloat. Sizing is thus *determined*,
   not guessed.
4. **`zstd`** produces `rootfs.img.zst` — the artifact a user actually
   downloads; the hypervisor mounts the raw `.img`. Both get a `.sha256`.
5. A `dpkg -l` manifest read straight from the tree's dpkg db (`--admindir`,
   no chroot), and chroot spot-checks.

The container is `--privileged` only because `mmdebstrap`'s root mode and the
chroot checks bind-mount `/proc,/sys,/dev` — not for the image creation itself.

Run it:

```sh
./build.sh                 # arm64, the macOS guest
ARCH=amd64 ./build.sh      # x86_64, the Windows guest
just guest-rootfs          # the same two, from the repo root
just guest-rootfs amd64
```

Everything is env-overridable (`SUITE`, `MIRROR`, `ARCH`, `ZSTD_LEVEL`,
`FS_UUID`, `BASE_IMAGE`). The multi-GB tree and apt cache live only in the
container's ephemeral layer (`--rm`) and are reclaimed on exit; only the final
artifacts are written to `out/` (a bind mount onto the host).

### What this pipeline does NOT do

**It does not produce a VHD or VHDX.** The output is a raw ext4 image on both
architectures, which is what §7 fixes as the on-disk format and what the
overlayfs lower/upper split is described against. Hyper-V does need the image
wrapped — as a *fixed* VHD, which is that same byte stream with a 512-byte
footer appended — and that wrapping is done **in Rust by the Windows backend at
first boot**, from these very bytes. Doing it here as well would ship a second
format, double the download, and give the two platforms two different artifacts
to checksum for no gain. If you came here to add it: it is already done, one
layer up.

### mmdebstrap over debootstrap; `minbase`; Recommends off

- **mmdebstrap**, as §9 specifies. It worked in-container with no fallback.
- **`--variant=minbase`**: Priority `required` + apt, and nothing else. Not
  `important`/`standard` (they pull an `ubuntu-minimal`-ish set we do not want),
  and emphatically not `buildd` (that *is* `build-essential`). This is the
  leanest base that still has a real userland (`passwd`, `util-linux`,
  `base-files`) — which the engine's fresh-UID-per-exec jail (§5) wants.
- **`Apt::Install-Recommends "false"`**: Recommends is the channel through which
  compilers, `-dev` headers, and desktop/X integration would sneak in. It is
  off; everything genuinely needed is named explicitly in `packages.txt`; and
  `verify.txt` asserts that no `gcc/g++/make/build-essential/dpkg-dev/binutils/
  libc6-dev` landed.
- **`--components=main,universe`**: csvkit, ripgrep, the tesseract language data,
  and the metric-compatible fonts live in `universe`.

### Reproducible-ish

The image carries a **fixed UUID and hash_seed** with eager table
initialisation, so rebuilds do not churn the bytes for cosmetic reasons. The
suite and package set are pinned, and `packages-manifest.txt` records the exact
version of every package pulled — that file *is* the pin. For bit-for-bit
reproducibility, point `MIRROR` at a `snapshot.ubuntu.com` timestamp instead of
the live mirror; the manifest then documents which snapshot. (The live mirror
depends on the architecture and the two do not overlap: arm64 packages are
served from `ports.ubuntu.com/ubuntu-ports` and amd64 packages from
`archive.ubuntu.com/ubuntu` — a common trap, which is why both scripts derive
the mirror from `ARCH` rather than let a caller pair them wrongly.)

## Corrections to §7's package names

§7 is prose, not a package list. Translating it to real noble package names:

1. **No `libreoffice-core-nogui`.** That package does not exist. "Headless" is a
   *runtime flag* (`soffice --headless`), not a build flavour. The headless set
   is `libreoffice-writer` + `libreoffice-calc` + `libreoffice-impress` +
   `libreoffice-core`; with Recommends off, the GUI integration packages
   (`libreoffice-gtk3`, `libreoffice-gnome`) are simply never pulled. This gives
   full headless docx/xlsx/pptx read, write, and `--convert-to`.
2. **No `python3-pptx` in Ubuntu 24.04 *or* 26.04.** python-pptx is genuinely
   not packaged for either release (universe included; no alternative name;
   re-checked on resolute). It is **dropped** to keep the image apt-pure and
   reproducible: a build-time `pip` would make the rootfs depend on a resolver
   run rather than on a package set, and it is the *reproducibility* of the
   image that argument protects, not the guest's inability to fetch. **pptx is
   not lost**: LibreOffice headless reads and writes pptx via `--convert-to`; only
   the Python *binding* is absent. If a native binding is later required, the
   clean fallback is to `apt install` its dependencies (`python3-lxml`,
   `python3-pil`, `python3-xlsxwriter`) and then `pip install --no-deps
   python-pptx` as a single documented build-time step, removing pip afterward —
   deliberately *not* the default.
3. **`iconv`** is not its own package; it ships in `libc-bin` (as §7 notes),
   which is listed.
4. **Full locales** are provided by `locales-all` (every locale pre-compiled),
   so there is no `locale-gen` step to run in a network-less image.
5. The **fonts-noto family** is spelled as its real split packages:
   `fonts-noto-core`, `-extra`, `-ui-core`, `-cjk`, `-color-emoji`.
   `fonts-noto-cjk` is large; it is included on purpose so a UK university's
   international correspondence renders without tofu (the §7 concern), but it is
   the first thing to drop if image size ever bites.
6. The metric-compatible MS substitutes are `fonts-crosextra-carlito`
   (= Calibri) and `fonts-crosextra-caladea` (= Cambria) — the pair that makes a
   converted university letter reflow the same as the original.

## Sizes and results

As built on an Apple-Silicon Mac (podman, native arm64), Ubuntu 26.04 LTS
(resolute) / arm64. The amd64 image is the same package set from the same
suite, so the numbers should land close — but no figure below has been measured
on x86_64, and none is asserted for it:

| Quantity | Value |
|----------|-------|
| Packages installed (`dpkg`) | **402** |
| Package tree (apparent) | 1,978 MiB |
| Content in ext4 (used blocks) | 2,095 MiB |
| **`rootfs.img`** (snug ext4, `resize2fs -M`) | **2,198,302,720 B = 2,096 MiB (2.20 GB)** |
| — trimmed vs. naive sizing | 730 MiB removed |
| **`rootfs.img.zst`** (zstd -19, the download) | **510,343,471 B = 487 MiB** (ratio 4.31) |
| `rootfs.img` SHA-256 | `2a45b3fae66efc5c65fe6cdb880718b348c8209055901d234d23290f4e9a114f` |
| `rootfs.img.zst` SHA-256 | `49d6ad93823a2755a7c3e1f190f208af6a36689fb4f8d132284a623b4224d4ed` |
| Build wall time (assemble → mkfs → verify) | ~245 s |

Spot-checks (`out/verify.txt`): Python stack imports (pandas 2.3.3, Pillow
12.1.1, plus openpyxl / python-docx / pypdf); pandoc 3.7.0.2; LibreOffice
26.2.2.2 present; tesseract languages eng/fra/deu/spa/ita/por/nld (+osd);
qpdf 12.3.2, ripgrep 15.1.0, ocrmypdf 16.13.0; and **no toolchain present**.

One caveat, stated honestly: the `soffice` headless `txt→pdf` spot-check does
**not** complete within a bounded timeout in the build container — soffice's
cold start busy-loops in a constrained chroot (4 vCPUs, no real init). This is
an environment artifact, not an image defect: LibreOffice is installed and
version-verified, and the fontconfig cache is pre-baked so the dominant
first-run font scan is already paid. The convert path is expected to work in the
real guest VM (its own kernel, full init, more resources). The check is marked
non-fatal.

> SHA-256 values above are for the specific build recorded here; a rebuild
> against the live archive will differ until the recipe is pinned to a
> `snapshot.ubuntu.com` timestamp (see *What full determinism would take*).

## Open questions

### Image format: ext4 now, squashfs later?

The lower layer is read-only, so a **squashfs** image would be the natural fit:
compressed on disk (much smaller at rest than even a snug ext4), mounted
directly with no unpack step, and inherently immutable. We are **not** taking it
now for one concrete reason: §7 fixes the on-disk format as *raw ext4 images*
because the Windows path hands Hyper-V the same bytes wrapped as a **fixed VHD**
(a footer, appended at first boot — see *What this pipeline does NOT do*), and
the overlayfs lower/upper split is described against ext4 block devices. A
squashfs lower would need both the wrapping and the overlay stories re-examined
on Windows. It stays a live option — the `.zst` we ship already recovers most of
the at-rest size win for the download — but the format is load-bearing for the
Windows port, so changing it is not a rootfs-only decision.

### Distribution: download the CI artifact, or build it on the user's machine?

Today the model is: CI builds this image once, publishes `rootfs.img.zst` + its
`sha256`, and every install downloads it. An alternative Alex is weighing is to
have the **installer build the rootfs locally** instead:

- **Shape.** A tiny pinned builder VM — the shipped kernel plus a small
  `mmdebstrap` initramfs — runs *this same recipe* against a snapshot-pinned
  mirror. With reproducible-mode `mmdebstrap` and deterministic `mkfs.ext4`
  (fixed UUID, `hash_seed`, `SOURCE_DATE_EPOCH`), every machine yields a
  **byte-identical** image, verifiable against the *same* published `sha256` as
  the CI artifact. Package integrity rides apt's existing signature chain.
- **Costs, honestly.** Ten-plus minutes of apt at install time; a real
  install-time failure surface (flaky mirrors, corporate proxies, and sites that
  block Ubuntu mirrors outright — those need the prebuilt artifact regardless);
  and reproducibility corner cases that CI otherwise absorbs by building once.
- **Position.** The CI artifact stays canonical. A local build is a possible
  *second* path over the same pinned recipe — not a replacement.

### What full determinism would take

The pipeline is already **reproducible-ish**: fixed suite + package set, a
`dpkg -l` manifest that records every exact version, a fixed FS UUID and
`hash_seed`, and eager (non-lazy) inode/journal init so `mkfs` output does not
churn. To make it **byte-for-byte** deterministic — the precondition for the
verify-by-shared-sha256 story above — the recipe would additionally need:

- **`snapshot.ubuntu.com` pinning** instead of whichever live archive the
  architecture selects, so the exact package bytes are frozen at a timestamp
  rather than whatever the mirror serves today (point `MIRROR` at a snapshot
  URL; it overrides the derived one on both arches).
- **`SOURCE_DATE_EPOCH`** honoured through every stage that stamps a time
  (mmdebstrap supports it; it is already pinned as an env var here) so
  timestamps in the tree are fixed.
- **`mkfs.ext4` determinism flags** — the fixed UUID / `hash_seed` / eager-init
  set is already applied; a fully reproducible run would also want to confirm
  `resize2fs -M` and any e2fsprogs version differences do not perturb the
  layout (pin the builder image's e2fsprogs version).

These are **notes, not implemented here** — the current build targets the live
archive for reliability and records versions in the manifest.

## `boot.img`: Ubuntu's own kernel, plus this repo's own initramfs

`rootfs.img` is only the userland; to boot, the guest also needs `boot.img` —
still **built by a different script** (`build-boot.sh`, this directory) since
it ships with the synod app rather than being downloaded like `rootfs.img`
(§7), but resolved here because the two pipelines share one pinned suite, one
`ARCH` knob, and one container of the guest's own architecture.

**The kernel is Ubuntu's stock `linux-generic` build for the guest's
architecture, unmodified — not a custom kernel.** It is already built, signed,
and validated by Canonical's own pipeline across the exact feature set the two
hypervisor paths need between them — virtio and virtiofs on one side, Hyper-V
(`CONFIG_HYPERV`, `CONFIG_HYPERV_VSOCKETS`) and 9p on the other, `vsock`,
overlayfs and ext4 on both — so lifting it trades zero engineering cost for a
small boot-time module-load cost that a from-scratch kernel config and rebuild
would only marginally improve — while creating a liability (kernel config
maintenance, security patch tracking) this project would then own forever.
`build-boot.sh` extracts `vmlinuz` and the needed `.ko` set straight from the
pinned suite's own `linux-image-generic` package for that architecture, inside
the same container that builds `rootfs.img`, so kernel and userland share one
pinned suite and mirror.

**What the two loaders want differs, and the script is asymmetric on purpose:**

- **arm64.** `VZLinuxBootLoader` on Apple silicon boots only a raw
  uncompressed ARM64 `Image`, and Ubuntu ships `vmlinuz` wrapped — a
  unified-image PE whose kernel is its `.linux` section, an EFI zboot PE, and a
  compressed payload, nested differently per suite. So the arm64 path *peels*:
  one layer at a time, refusing any format it does not recognise, until `file`
  agrees it is an `ARM64 boot executable`.
- **amd64.** Hyper-V's `LinuxKernelDirect` loader boots a **bzImage**, and
  Ubuntu's `vmlinuz-*-generic` for amd64 already *is* one. So the amd64 path
  unwraps **nothing**: the bytes are copied through and asserted to be a "Linux
  kernel x86 boot executable bzImage". Peeling here would be actively wrong —
  it would yield a bare ELF `vmlinux` that no loader will start.

That asymmetry is the kind of thing that reads as an oversight, so it is spelled
out in the script at the point of use as well as here.

**Neither `ral-daemon` nor the engine live in `rootfs.img`** (this directory's
own build never puts them there). What makes `boot.img`'s "kernel +
ral-daemon + ral engine as one artifact" true is a small hand-written
initramfs, `ral-initramfs` (a workspace crate, `libc` + `rustix` only, the
same dependency shape as `ral-daemon`): its job is fixed and narrow enough —
two known disks in the backend's own fixed attach order (virtio-blk on macOS,
two LUNs of one SCSI controller on Hyper-V, so the *names* are probed while the
order is not), one overlay, one binary-install step, one `switch_root` — that a
general-purpose tool like dracut or mkinitramfs would be solving a
hardware-discovery problem this guest does not have. On every boot it:

1. mounts the kernel's device nodes at `/dev` (claimed, not assumed from a
   devtmpfs automount) and loads whichever kernel modules the pinned kernel
   build needs for the two disks, the overlay, and vsock (a manifest
   `build-boot.sh` writes from the real `CONFIG_*` the kernel package ships,
   not a hardcoded guess);
2. `mke2fs`-formats the session disk unconditionally — it always arrives as
   a bare zero-filled sparse file (`vm-manager`'s `create_session_image`);
3. mounts `rootfs.img` read-only and the freshly formatted session disk
   read-write as an overlay's lower and upper;
4. **copies the embedded `ral-daemon` and `exarch` binaries onto that
   writable upper**, at `/sbin/ral-daemon` and `/usr/libexec/ral/engine` —
   the mechanism behind the "one artifact" claim: the binaries are installed
   fresh into the session overlay every boot, never baked into the read-only
   rootfs;
5. `switch_root`s into the assembled overlay and execs `/sbin/ral-daemon` as
   the new pid 1.

Both `ral-daemon` and `exarch` are cross-built for the guest's musl target
(`aarch64-unknown-linux-musl` or `x86_64-unknown-linux-musl`) *inside* the same
container (rustup + `musl-tools`, the recipe `build-binaries.yml`'s
`build-exarch-linux-arm64` job already uses — no `cross-rs` container needed,
since neither binary pulls `jemalloc-sys` or a C dependency), and the build
refuses any of the three that is not a static binary of the guest's own
architecture. `mke2fs` is the one non-Rust piece the initramfs carries: it is
vendored from the container's own `e2fsprogs` package together with every
shared library `ldd` reports, resolved in the same pinned container so glibc
versions match exactly — the technique every distro's own initramfs tooling
already uses internally.

`vm-manager/src/vz.rs`'s `BootArtifact { kernel, initramfs, rootfs }` and
`kernel_command_line()` need no changes for any of this: the command line
already never sets `ral.engine=`, so as long as `build-boot.sh` installs the
engine at `ral-daemon`'s own `DEFAULT_ENGINE` path, the existing contract
holds untouched — and the Windows backend takes that same `BootArtifact`, three
paths and nothing hypervisor-shaped about them.

### Which modules each guest carries, and why

The module set is the one part of `boot.img` that is genuinely per-guest, so it
is *derived*, never asserted: `build-boot.sh` records the real `CONFIG_*` value
of every symbol it cares about into `kernel-config-check.txt`, ships only the
ones the pinned kernel package built as `=m` (a `=y` needs nothing shipped),
orders them by `modules.dep`'s own transitive lines, and **fails the build** if
a name in its table is `=m` yet absent from `modules.dep` — the signature of a
module renamed under a kernel bump, caught at build time instead of at
`mount(2)` in a guest nobody can log into.

So the table below is the *question* the build asks of the kernel config, not a
list of files: a driver named here is carried only if this kernel built it as a
module, and several are compiled in.

| Concern | arm64 (Virtualization.framework) | amd64 (Hyper-V) |
|---|---|---|
| Bus / devices | `virtio`, `virtio_ring`, `virtio_mmio`, `virtio_pci`, `virtio_blk` | `hv_vmbus`, `hv_storvsc`, `hv_utils`, `hv_balloon` |
| Control plane (`AF_VSOCK`) | `vsock` + `vmw_vsock_virtio_transport` | `vsock` + `hv_sock` |
| Granted folder | `virtiofs` | `9p` + `9pnet` + `9pnet_fd` |
| Overlay + disks | `overlay`, `ext4` | `overlay`, `ext4`, `scsi_mod`, `sd_mod` |
| Console | `hvc0` (`virtio_console`) | `ttyS0` (8250, compiled in) |

The amd64 column earns its differences one at a time:

- **`hv_vmbus` is the bus.** Nothing else in that column is reachable until it
  has enumerated; `hv_storvsc` is the SCSI HBA both disks arrive behind,
  `hv_utils` the KVP/shutdown/timesync services the host expects a well-behaved
  guest to answer, `hv_balloon` its dynamic-memory client.
- **There is deliberately no `hv_netvsc`.** Ubuntu builds it, and no hypervisor
  here gives the guest a paravirtualised NIC (§6) — its network arrives over
  `tun`, not the VMBus — so shipping its driver would load code for hardware
  that is not there. `CONFIG_HYPERV_NET` is still *recorded* in
  `kernel-config-check.txt`, so the artifact shows a decision rather than an
  omission.
- **The transports are mirror images.** `CONFIG_HYPERV_VSOCKETS` is recorded and
  declined on arm64; `CONFIG_VIRTIO_VSOCKETS` is recorded and declined on amd64.
  Above either one the guest speaks the same `AF_VSOCK` API, which is the whole
  reason §2 can promise "the guest-side code is identical on both".
- **The workspace arrives differently.** There is no virtiofs on Hyper-V, so the
  granted folder is a **9p (9p2000.L) share the host serves over a vsock port**:
  `9p` (the filesystem) over `9pnet` (the protocol core) over **`9pnet_fd`**.
  That last module is the trap worth naming: `trans=fd` used to be compiled
  *inside* `9pnet`, and in a kernel new enough to have split it out
  (`CONFIG_NET_9P_FD`, upstream `9pnet_fd-objs := trans_fd.o`) a mount with
  `9pnet` loaded and `9pnet_fd` missing fails with a bare `ENODEV`. Ubuntu's
  resolute kernel (7.0.0) builds all three as modules, and `9p` pulls the
  `netfs` core in through its own `modules.dep` line — so a resolute amd64 build
  ships eleven files: `hv_vmbus`, `hv_storvsc`, `hv_utils`, `hv_balloon`,
  `vsock`, `hv_sock`, `9pnet`, `9pnet_fd`, `netfs`, `9p`, `overlay`.
- **SCSI and ext4 are compiled in.** `CONFIG_SCSI`, `CONFIG_BLK_DEV_SD` and
  `CONFIG_EXT4_FS` are `=y` in Ubuntu's amd64 config, so nothing is shipped for
  them — but they are named in the table anyway, so that a kernel which makes
  any of them a module ships it, resolved from `modules.dep` like everything
  else rather than assumed absent forever.
- **The console needs nothing.** `CONFIG_SERIAL_8250` is `=y`, and the *host*
  writes the kernel command line, so `boot.img` is neutral about whether it is
  handed `console=hvc0` or `console=ttyS0`.

Run it; outputs land in `vm-image/out/boot/`:

```sh
./build-boot.sh                 # arm64, for Virtualization.framework
ARCH=amd64 ./build-boot.sh      # x86_64, for Hyper-V
just guest-boot                 # the same two, from the repo root
just guest-boot amd64
```

The products are `kernel`, `initramfs.img`, a `.sha256` for each,
`boot-manifest.txt` (the architecture, on its own leading line, plus the kernel
package version and the git commit hash of the ral-daemon/exarch source built in
— recording the hash only here, not in a `build.rs`, is the whole of the
version-stamping decision), `kernel-config-check.txt` (the real `CONFIG_*`
values the config-grep pass found), `verify.txt` (which asserts the kernel's
format, the three binaries' architecture and static linkage, and lists the
shipped modules in load order), and `build.log`.

**Smoke-boot is human-in-the-loop, not CI**: on macOS, build `rootfs.img` and
`boot.img`, run `dev/scripts/sign-virtualization.sh` against the debug
`vm-manager`/`synod` binaries (macOS invalidates the ad-hoc signature on every
rebuild), then boot via `vm-manager/examples/boot-smoke.rs` with the three
real artifacts, and confirm on stdout `ral-daemon`'s own `eprintln` lines
(`guest filesystems up`, `engine running as pid`), that `announce_root()`
reports the session overlay rather than a fallback, and that the guest dials
the host's vsock control port within `vz.rs`'s 30s `BOOT_TIMEOUT`. On Windows
the same smoke-boot needs a machine with Hyper-V enabled (Pro, Education, or
Enterprise — §2) and an elevated shell, since creating a machine through the
Host Compute System API is an administrative act; the media itself is verified
without booting either way (`verify.txt`).

### Open past this point

- **Exact module set vs. the real kernel config**: `build-boot.sh`'s
  config-grep pass records what the pinned kernel package actually built
  (`kernel-config-check.txt`); which virtio transport
  (`virtio_mmio`/`virtio_pci`) Virtualization.framework actually attaches on
  is confirmed only by the smoke-boot above, not by this non-booting pass —
  both are shipped if both are modules, so whichever it is, is already
  loaded.
- **The amd64 table's provenance**: every module name in it was checked against
  the file list Ubuntu's own `linux-modules-7.0.0-14-generic` (amd64, resolute)
  ships and against that kernel's `debian.master/config/annotations` for `=m`
  versus `=y` — not against a `modules.dep` produced by a build here, because
  no amd64 build has been run yet on the fleet. That is exactly what the
  build's hard failure exists for: a drifted name stops the build, and it
  cannot ship a boot artifact that would fail at `mount(2)` instead.
- **Windows boot path**: no longer a Gen-2 UEFI unified kernel image. Hyper-V's
  `LinuxKernelDirect` loader takes the bzImage and this initramfs directly, the
  way `VZLinuxBootLoader` takes the `Image` — so the UKI step §2 anticipated is
  simply not needed. What remains open is validation on real hardware: a
  Windows box with Hyper-V, per §10.
