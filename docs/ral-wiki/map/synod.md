---
generated_at_commit: f7cf93a
generated_at_date: 2026-07-25
covers_paths: [synod/, vm-manager/, ral-daemon/, vm-image/]
---

# Map: synod

synod is a second product over the same engine — an *office-work* delegate
where [[map/exarch|exarch]] is a coding one. The user grants one folder,
describes a task in plain English, and the agent works in it — the folder
itself, in place, under a safety net: checkpoint before the job, a
plain-language change report after it, conflict-checked undo per file or whole
job. It is not a fork of exarch and not a mode of it: it depends on exarch as
a library and supplies only what differs
([[decisions/260721_synod-is-a-second-product|synod-is-a-second-product]]).
And it is itself a library, not a binary: `synod-app`, the GUI, is the one
process.

The design record is `dev/docs/VM/SYNOD.md`; the landed state is
`dev/docs/VM/SYNOD-v1.md`. The VM stack runs end to end: every conversation
boots a real Virtualization.framework machine from shipped boot media — there
is no software-only fallback — and the engine runs inside the guest, one
engine process per session, driven over the design's §3 wire
([[decisions/260722_session-is-a-process|session-is-a-process]]).

## synod/ — the library

- `lib.rs` — the crate doc names the five differences from exarch;
  `boot_media()` finds the shipped kernel/initramfs/rootfs (the bundle's
  `Resources/boot/`, or `vm-image/out` in development) for
  `vm_manager::detect`.
- `grant.rs` — the folder becomes a `ral_core::types::Capabilities`
  ([[design/grant|grant]]), and a `vm_manager::MachineSpec` naming the same
  folder. One grant, read twice: once as authority, once as a workspace.
- `prompt.rs` + `data/*.md` — the office persona and the office toolbox,
  assembled through exarch's own section renderer (`exarch::prompt::render`)
  over exarch's environment-and-grant section (`exarch::prompt::host_section`).
- `session.rs` — `Conversation`, one folder held open from first message to
  last. `begin` opens the grant, boots the machine, and seats exarch's agent
  on the wire the machine hands back (`exarch::agent::RootSeat::Wire` over
  `Machine::take_control`, attached at the guest's `/work`); `exchange`
  drives one message through `exarch::headless::converse_sink`, bracketed by
  the safety net — a checkpoint before, a checkpoint after, even after a
  failed run; `end` closes the wire, and the guest halts itself. The model
  picker lives here too: `menu`/`refresh_menu` list what the computer's
  credentials can reach (cached-instant and fetched-complete), and a
  `Choice` names provider, model, and effort. `sign_in` drives exarch's
  browser login flow (`exarch::provider::oauth::login_flow`) and admits the
  fresh account to the live store and catalog, so a ChatGPT plan signed in
  from the window is usable without a restart — the credential store is
  behind a `Mutex` for exactly that reason, taken only for an account list
  or an admission, never across a fetch or a boot.

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
cargo) is the one process: it holds the `Conversation` in-process — no child
binary, no stdin framing. One window, three states: choose a folder and a
model (with a Thinking control beside the Assistant picker) and describe the
job; watch the assistant work, its narration streamed in; then read what
changed and put anything back. `commands.rs` holds the folder picker, the
conversation verbs (start, send, restart, end), the model listing (instant
from the cache, one background refresh), and opening before/after versions
with the user's own applications; `signin.rs` runs the opening screen's
"Sign in with ChatGPT" button — one sign-in at a time, cancellable, its
progress and outcome events (`sign-in-step`, `sign-in-done`) rendered beneath
the button, and the account it wins arriving as the same `models-refreshed`
the picker already renders through; with no account set up the sign-in is the
screen's primary button and the folder picker waits for it; `review.rs`
translates the workspace
vocabulary into cards and runs the gentle-then-explicit conflict flow;
`main.rs` runs exarch's `dispatch_pre_main` re-exec trampoline first, like
every [[invariants/single-binary|multicall]] binary here.

## vm-manager/ — the machine

One trait each side of a boot: `Hypervisor::boot(&MachineSpec) -> Box<dyn
Machine>`, with `MachineSpec::resolve` the one platform-independent judgment
of a spec, called by every backend so a bad spec is refused in the same words
everywhere.

**The crate boots only real machines.** `detect(Option<BootArtifact>)`
answers `Vz` or refuses with a sentence for a non-programmer — not an Apple
Silicon Mac, no boot media, or a build unsigned for virtualization. There is
deliberately no software fallback: a synod that cannot put hardware between
the agent and the rest of the computer refuses to start rather than degrade
to a weaker mode.

- `vz.rs` — `Vz`: Virtualization.framework, macOS arm64, bound through
  `objc2-virtualization`. It builds and validates the full configuration —
  direct kernel boot, RO rootfs + RW sparse session disks, a virtiofs share
  of the granted folder with `read_only` as the mount's law, a vsock device
  and no network device, console to the host log — drives the `!Send`
  machine from a dedicated thread against a private serial dispatch queue,
  and declares boot only when the guest's daemon dials the control port.
  That accepted connection is the host end of the §3 control plane:
  `Machine::take_control` hands it out exactly once (a second ask panics as
  a caller's bug), and a second guest dial is refused. Booting requires the
  `com.apple.security.virtualization` entitlement — `vz::entitled()` is a
  process check, not a platform check.

## ral-daemon/ — the guest's PID 1

Runs inside every booted guest. Every decision — the kernel-cmdline
`boot::Boot`, the mount plan and its inside-before-outside ordering invariant
(`mounts.rs`), the guest-wide sysctls the §5 jail depends on (`sysctl.rs`),
the engine's command line and fd plumbing (`engine.rs`: the vsock connection
arrives as fd 3), the classification of a wait result (`reap.rs`) — is a pure
function unit-tested on any machine; the syscalls are a thin edge only a
guest can exercise. The overlay root is deliberately the initramfs's job; the
daemon verifies and names what it was handed. No ral semantics, no authority
policy. When the engine exits — the wire's EOF is its cue — the daemon powers
the machine off from inside: the clean inside-out halt.

## vm-image/ — the boot media

The design record's §7 built. `build.sh` assembles the rootfs — a pinned
**Ubuntu 26.04 LTS (resolute) arm64** office userland (LibreOffice headless,
the Python document stack, pandoc, OCR, wide fonts, full locales, no
toolchain) via mmdebstrap → ext4 → zstd in a native-arm64 container,
checksummed and version-manifested. `build-boot.sh` builds the boot pair:
Ubuntu's generic kernel taken apart to the raw Image `VZLinuxBootLoader`
boots, and a hand-written initramfs whose every decision is a typed plan —
assemble the overlay root, make the session disk, install daemon and engine,
`switch_root` — with the git hash stamped into `boot-manifest.txt`. The
README records the corrections of §7's prose to real package names and the
open questions (squashfs, distribution, determinism).

## What is not here

The Windows Hyper-V backend, and the image pipeline's open questions above.
The rest of the design record runs: the §3 wire carries real runs
(`synod/examples/boot-run.rs` witnesses boot → virtiofs share → engine →
settled report), the §5 spawn jail stands inside the guest (a fresh uid and
a cgroup between the engine and what it runs), and §6's `fetch-url` answers
the web through `exarch::fleet::egress` under IT's policy, threaded once
through `RootConfig` so both seats answer alike.
[[decisions/260715_vm-workspaces-cross-by-copy|vm-workspaces-cross-by-copy]]
records where synod's work-in-place workspace deliberately departs from
exarch's cross-by-copy position.

## Where to look

- `dev/docs/VM/SYNOD.md` — the design record; `dev/docs/VM/SYNOD-v1.md` —
  what landed; `dev/docs/VM/EXARCH-VM-v2.md` — the shared VM implementation
  plan; `vm-image/README.md` — the boot-media pipeline and its open
  questions.
- [[map/exarch|exarch]] — the sibling binary and the library synod embeds.
- [[design/exarch-architecture|exarch-architecture]] — the loop both products
  run.
