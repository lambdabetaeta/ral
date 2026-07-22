---
generated_at_commit: b675160
generated_at_date: 2026-07-21
covers_paths: [synod/, vm-manager/, ral-daemon/, vm-image/]
---

# Map: synod

synod is a second agent binary over the same engine — an *office-work* delegate
where [[map/exarch|exarch]] is a coding one. The user grants one folder,
describes a task in plain English, and the agent works in it — the folder
itself, in place, under a safety net: checkpoint before the job, a
plain-language change report after it, conflict-checked undo per file or whole
job. It is not a fork of exarch and not a mode of it: it depends on exarch as
a library and supplies only what differs
([[decisions/260721_synod-is-a-second-product|synod-is-a-second-product]]).

The design record is `dev/docs/VM/SYNOD.md`; what actually landed — the safety
net and desktop shell running under `Boundary::None`, a VM stack (rootfs
image, Virtualization.framework backend, guest daemon) built, and now the
design's §3 frame protocol with heartbeat liveness, though no engine yet runs
inside a guest — is `dev/docs/VM/SYNOD-v1.md`.

## synod/ — the application

`run` (in `lib.rs`, lifted out of `main` so tests link the whole crate) is
three steps: open the granted folder, boot a machine to hold it, start the
session. `main.rs` runs exarch's `dispatch_pre_main` re-exec trampoline first,
like every [[invariants/single-binary|multicall]] binary here.

- `cli.rs` — argv: the folder, the job, and the session knobs.
- `grant.rs` — the folder becomes a `ral_core::types::Capabilities`
  ([[design/grant|grant]]), and a `vm_manager::MachineSpec` naming the same
  folder. One grant, read twice: once as authority, once as a workspace.
- `prompt.rs` + `data/*.md` — the office persona and the office toolbox,
  assembled through exarch's own section renderer (`exarch::prompt::render`)
  over exarch's environment-and-grant section (`exarch::prompt::host_section`).
- `session.rs` — provider, prompt, `exarch::agent::Agent`, the run driven to
  quiescence through `exarch::headless::run` — bracketed by the safety net: a
  jargon-free notice of what is about to happen when there is no hardware
  wall (unit-tested to speak English), a checkpoint before the job, a
  checkpoint and rendered change report after it, even after a failed run.

## synod/src/workspace/ — the safety net

The module the product's guarantee lives in, all host-side, exercised under
ordinary `cargo test`:

- `manifest.rs` — a folder's state at one moment: path, kind, size, blake3
  hash; empty folders and symlink targets recorded, links never followed; a
  cheap `measure` for the large-folder warning (~2 GiB).
- `history.rs` — the per-folder store: content-addressed `objects/`
  (identical bytes kept once, ever) plus `checkpoints/<id>.json`, with
  `Before`, `After`, and `Undo` moments. Every byte a job or an undo replaces
  is in the store before it is touched.
- `changes.rs` — the delta between two manifests: created, modified, deleted,
  renamed (a deleted and a created file with identical bytes, paired).
- `restore.rs` — the conflict-checked driver: a path edited *after* the job
  is a conflict resolved only by the caller (`KeepCurrent` or explicit
  `PutBack`); nothing silently overwritten, nothing destroyed.
- `report.rs` — the GUI's seam: the job report, `undo_file` (either name of a
  rename undoes both sides), `undo_all`, the headless run's plain-text
  rendering.

## synod/app/ — the window

The desktop shell (Tauri v2, hand-written static frontend, no bundler, pure
cargo). One window, three states: choose a
folder and describe the job; watch the assistant work — the `synod` CLI
spawned as a child, output streamed in, never linked in-process; then read
what changed and put anything back. `review.rs` translates the workspace
vocabulary into cards and runs the gentle-then-explicit conflict flow;
`commands.rs` holds the folder picker, the child run, and opening
before/after versions with the user's own applications.

## vm-manager/ — the machine

One trait each side of a boot: `Hypervisor::boot(&MachineSpec) -> Box<dyn
Machine>`, with `MachineSpec::resolve` the one platform-independent judgment
of a spec, called by every backend so a bad spec is refused in the same words
everywhere.

**`Boundary` is the crate's load-bearing type: a machine is asked what stands
between its agent and the rest of the computer, and may answer only
`Hardware` or `None`.** There is deliberately no third variant meaning
"partly". It exists so the application can tell a non-programmer the truth
instead of implying a wall.

- `host.rs` — `Host`: no machine at all. It resolves the folder, hands it
  back where it lies, and reports `Boundary::None`; enforcement is then
  entirely ral's [[internals/capability-enforcement|two enforcers]]. This is
  what runs today, on every platform.
- `vz.rs` — `Vz`: Virtualization.framework, macOS only, **real code** (no
  longer a `todo!()` seam), bound through `objc2-virtualization`. It builds
  and validates the full configuration — direct kernel boot, RO rootfs + RW
  sparse session disks, a virtiofs share of the granted folder with
  `read_only` as the mount's law, a vsock device and no network device,
  console to the host log — drives the `!Send` machine from a dedicated
  thread against a private serial dispatch queue, and declares boot only when
  the guest's daemon dials the control port. That accepted connection is
  dupped and held as the host end of the §3 control plane:
  `Machine::take_control` hands the descriptor out at most once (`Host`
  answers `None`), and a second guest dial is refused. It starts
  machines only from a signed binary with the
  `com.apple.security.virtualization` entitlement and
  a real boot image, so `detect()` still returns `Host` — the switch to `Vz`
  is a deliberate later act, not a platform check.

## ral-daemon/ — the guest's PID 1

Real code, Linux-only in body: every decision — the mount plan and its
inside-before-outside ordering invariant, the engine's command line and fd
plumbing (the vsock connection arrives as fd 3), the classification of a wait
result — is a pure function unit-tested on any machine; the syscalls are a
thin edge only a guest can exercise. The overlay root is deliberately the
initramfs's job; the daemon verifies and names what it was handed. No ral
semantics, no authority policy. Nothing that runs today executes it.

## vm-image/ — the rootfs

The design record's §7 built: a pinned **Ubuntu 26.04 LTS (resolute) arm64**
office userland — LibreOffice headless, the Python document stack, pandoc,
OCR, wide fonts, full locales, no toolchain — assembled by `build.sh`
(mmdebstrap → ext4 → zstd in a native-arm64 container), checksummed and
version-manifested. The README records the corrections of §7's prose to real
package names and the open questions (squashfs, distribution, determinism).
`boot.img` — kernel, initramfs, daemon, engine — is documented there and not
built.

## What is not here

The frame protocol of `SYNOD.md` §3 now exists — codec, `Attach` handshake,
enquiry desk, and heartbeat liveness, spoken by a re-exec'd engine child — but
the seam it would join is still open: `session.rs` drives exarch's agent
in-process, so no engine has run inside a guest. What is not here is the
front-end switch onto the wire, and `boot.img` (unbuilt), the §5 spawn jail,
§6's `fetch-url` verb with the org policy loader and egress audit, and the
Windows Hyper-V backend. What runs today runs under `Boundary::None`, with the
safety net as the product's actual protection;
[[decisions/260715_vm-workspaces-cross-by-copy|vm-workspaces-cross-by-copy]]
records where synod's work-in-place workspace deliberately departs from
exarch's cross-by-copy position.

## Where to look

- `dev/docs/VM/SYNOD.md` — the design record; `dev/docs/VM/SYNOD-v1.md` —
  what landed; `dev/docs/VM/EXARCH-VM-v2.md` — the shared VM implementation
  plan; `vm-image/README.md` — the rootfs pipeline and its open questions.
- [[map/exarch|exarch]] — the sibling binary and the library synod embeds.
- [[design/exarch-architecture|exarch-architecture]] — the loop both products
  run.
