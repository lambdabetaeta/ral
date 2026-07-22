# vm-image — the synod guest rootfs

This directory builds `rootfs.img`: the **read-only Ubuntu 26.04 LTS (resolute)
arm64 office userland** that synod boots inside its guest VM. It is one of the
three disks in SYNOD.md §7's stack:

> §7 pins **26.04 LTS**, the current LTS — a question this pipeline settled by
> validating the package set identically against 24.04 and 26.04. The suite
> stays a one-line env knob (`SUITE`); the translation of §7's prose into real
> package names is under *Corrections* below.

```
session.img  (RW, per-session)   overlayfs upper: /work + guest scratch
rootfs.img   (RO, pinned)   <--   THIS DIRECTORY BUILDS THIS ONE
boot.img     (ships w/ app)      kernel + ral-daemon + ral engine (NOT here)
```

## Why the image is the package manager

The guest has **no network device at all** (§6): office work is folder-local
and the model API is called from the host, so the guest is given only a virtual
socket. That single fact is the whole reason this directory exists. "No egress
means no installs at runtime, which makes the rootfs load-bearing: whatever
isn't in the image doesn't exist" (§7). So the image is deliberately rich — a
full document stack, a wide font set, full locales — and just as deliberately
lean where it counts: **no compilers, headers, or build systems**, because
"they exist only to install more software, and there is nothing to install
from."

No ral binaries live here either; the daemon and engine ship in `boot.img`,
versioned with the app so the `Attach` handshake can enforce the pairing (§7).

## Contents

| File | What it is |
|------|-----------|
| `packages.txt` | The pinned package set, one per line, grouped to mirror §7's rationale. Comments and blanks are stripped by the build. |
| `build.sh` | The whole pipeline: one strict-mode script that runs a container and emits the image, checksum, manifest, and verification. |
| `out/` | Build products (git-ignored). |

Build products in `out/`:

- `rootfs.img` — the ext4 image (what the hypervisor mounts)
- `rootfs.img.zst` — zstd-compressed image (what a user downloads)
- `rootfs.img.sha256`, `rootfs.img.zst.sha256` — checksums of both
- `packages-manifest.txt` — `dpkg -l` capture; the exact version-pin record
- `verify.txt` — spot-check results
- `build.log` — full build transcript

## How the pipeline works

macOS has neither `mmdebstrap` nor ext4 tooling, so the build runs entirely
inside a **native-arm64 `ubuntu:24.04` container** (arm64 is this Mac's native
arch, so no qemu emulation is involved). The 24.04 base only supplies the build
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
./build.sh
```

Everything is env-overridable (`SUITE`, `MIRROR`, `ARCH`, `ZSTD_LEVEL`,
`FS_UUID`, `BASE_IMAGE`). The multi-GB tree and apt cache live only in the
container's ephemeral layer (`--rm`) and are reclaimed on exit; only the final
artifacts are written to `out/` (a bind mount onto the Mac).

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
the live `ports.ubuntu.com` mirror; the manifest then documents which snapshot.
(arm64 packages are served from `ports.ubuntu.com/ubuntu-ports`, **not**
`archive.ubuntu.com` — a common trap.)

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
   re-checked on resolute). It is **dropped** to keep
   the image apt-pure and reproducible — the design leans on "the image is the
   package manager", and a build-time `pip` would undercut that. **pptx is not
   lost**: LibreOffice headless reads and writes pptx via `--convert-to`; only
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
(resolute) / arm64:

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
because the Windows path wraps that same image as a **VHDX** at install time,
and the overlayfs lower/upper split is described against ext4 block devices.
A squashfs lower would need the VHDX-wrapping and overlay stories re-examined on
Windows. It stays a live option — the `.zst` we ship already recovers most of
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

- **`snapshot.ubuntu.com` pinning** instead of the live `ports.ubuntu.com`
  archive, so the exact package bytes are frozen at a timestamp rather than
  whatever the mirror serves today (point `MIRROR` at a snapshot URL).
- **`SOURCE_DATE_EPOCH`** honoured through every stage that stamps a time
  (mmdebstrap supports it; it is already pinned as an env var here) so
  timestamps in the tree are fixed.
- **`mkfs.ext4` determinism flags** — the fixed UUID / `hash_seed` / eager-init
  set is already applied; a fully reproducible run would also want to confirm
  `resize2fs -M` and any e2fsprogs version differences do not perturb the
  layout (pin the builder image's e2fsprogs version).

These are **notes, not implemented here** — the current build targets the live
archive for reliability and records versions in the manifest.

## Still open: boot.img (documented, NOT built here)

`rootfs.img` is only the userland. To boot, the guest also needs `boot.img`,
which is **out of scope for this directory** (it ships with the synod app,
versioned with it — §7). What it will need:

- **An arm64 kernel** (macOS boots it directly via `VZLinuxBootLoader`; Windows
  wraps the *same* kernel as a Gen-2 UEFI unified kernel image — §2).
- **virtio guest drivers** for the Apple Virtualization.framework path
  (`virtio-blk`/`virtio-console`/`virtio-vsock`, etc.) **and Hyper-V guest
  drivers** for the Windows path (`hv_vmbus`, `hv_storvsc`, `hv_netvsc` — unused
  since there is no NIC, and crucially `hv_sock` so `AF_HYPERV` ↔ guest
  `AF_VSOCK` works). The guest-side vsock API is identical on both hypervisors;
  only the transport underneath differs (§2).
- **overlayfs** (RO `rootfs.img` lower + RW `session.img` upper — §7) and **ext4**
  built into the kernel.
- The **ral-daemon (PID 1)** and the **ral engine**, both static musl Rust (§7).

Ubuntu 26.04's stock **`linux-generic` arm64 kernel already provides all of
this**:
virtio and Hyper-V (`CONFIG_HYPERV`, `CONFIG_HYPERV_VSOCKETS`) drivers, overlayfs,
and ext4 are all in the generic config, mostly as modules. So `boot.img` can lift
Ubuntu's generic kernel (or build a slim custom one with these as built-ins to
skip an initramfs). Choosing generic-kernel-plus-initramfs versus a custom
built-in kernel is the real open question there — it does not affect this image.
