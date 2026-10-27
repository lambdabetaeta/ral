---
status: active
---

# The boot contract has a version, and the build is where skew is caught

**The `ral.` kernel-command-line keys and the grammar of their values are one
indivisible agreement between the host that writes the command line and the
guest that reads it, so they carry a single version — `ral_daemon::boot::CONTRACT`
— which the boot media records and `synod/build.rs` compares against the host it
is packaging: host/guest skew is a failed build, not sixty seconds of silence at
run time.** Amends the first open question of
[[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]]; realised in
`ral-daemon/src/boot.rs`, `ral-daemon/examples/boot-contract.rs`,
`vm-image/build-boot.sh`, and `synod/build.rs`.

## Context

An installed synod, built 2026-07-30, shipped guest boot media built 2026-07-25.
Between the two,
[[decisions/260727_the-guest-gets-a-network-not-a-verb|the-guest-gets-a-network-not-a-verb]]
added the key `ral.net` to the command line. `Boot::read` refuses an unknown
`ral.` key outright — a setting the guest cannot interpret is authority nobody
granted, and a misspelling silently ignored is worse than a boot that stops — so
the guest refused the whole line, powered off, and the person holding the
installer saw one sentence: *the guest did not dial the control plane within 60s
of starting*.

Two things that failure is not:

- **Not a refusal that was too strict.** The guest was right, and loudly so; the
  fault is entirely in *where* it was heard, which is
  [[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]'s
  subject.
- **Not a run-time check waiting to be written.** By the time a command line has
  been written there is nothing left to negotiate: the host cannot ask a guest
  which keys it knows without a channel the guest opens only after reading them.
  Every honest moment for the comparison is *before* the media and the host are
  packaged together.

## Decision

- **One number for the whole contract, not a per-key handshake.**
  `boot::CONTRACT: u32` is at 2. It is bumped whenever the key set or a value's
  grammar changes — a key added, a key retired, a value spelled a new way — and
  it says nothing finer, because the guest's own refusal is already all-or-
  nothing.
- **The number lives beside the command line's only writer and only reader.**
  `boot::CONTRACT` sits in `ral-daemon/src/boot.rs`, the module that both renders
  the line (`command_line`) and parses it (`Boot::read`), so a new key and its
  version cannot be added in two different commits.
- **The media records the number by compiling it, never by reading it.**
  `ral-daemon/examples/boot-contract.rs` prints `boot::CONTRACT` and nothing
  else; `vm-image/build-boot.sh` builds and runs it in the same cargo invocation,
  from the same checkout, that produces the `ral-daemon` going into
  `initramfs.img`, and writes `boot_contract=` into
  `vm-image/out/boot/boot-manifest.txt`. A grepped source line or a number kept
  by hand would record precisely the drift the number exists to catch; a compiled
  one can disagree with the shipped daemon only if the compiler disagrees with
  itself.
- **The comparison belongs to the build.** `synod/build.rs` reads that manifest
  — at the path `tauri.conf.json`'s resource map stages from, walked component by
  component so the refusal quotes something a person can paste — and hands it to
  `boot::check_media`. A mismatch is `exit(1)` with one sentence naming both
  numbers, the manifest, and `just guest-boot`; a panic would bury that sentence
  under a location pointing at the wrong file. `ral-daemon` is therefore a
  **build** dependency of synod rather than an ordinary one: the comparison is
  the build's business and synod's own code never asks.
- **Absent media is not a failure.** A `cargo check`, a test run, and a developer
  who has not spent an hour of podman on an image must all still compile the
  crate; missing media becomes an error at bundle time, where Tauri already names
  the resource it cannot find.
- **Three refusals, because three things can be wrong.** The numbers differ; the
  manifest carries no `boot_contract=` line at all, which means media of
  unknowable vintage rather than a known-bad version; or the line is there and is
  not a whole number, which is a broken manifest and not a mismatch.
- **The manifest reads like the command line it describes.** A key written twice
  takes its last value, and a key that merely starts with `boot_contract` is a
  different key.

## Consequences

- **Skew is caught on both bundles**, since `build.rs` belongs to the crate and
  not to a platform: the macOS `.app` and the Windows `.msi` are held to the same
  comparison by the same code.
- **The failure moves to the cheapest moment it has.** The stale `vm-image/out/`
  that produced a shipped installer now produces a red build on the machine that
  would have shipped it.
- **The version is a maintenance obligation, stated in one place.**
  `boot::CONTRACT`'s own doc comment is where the bump rule lives, and
  `check_media`'s tests pin the shape of all three refusals under ordinary
  `cargo test`, with no media and no machine.

## Open questions

- **Nothing enforces the bump.** A key added without touching `CONTRACT` leaves
  two ends that disagree while both claim contract 2, and the symptom is the
  original one. Co-location makes forgetting unlikely; it does not make it
  impossible, and a test that derives the number from the key set is the obvious
  next move.
- **An installed mismatch is still only legible after the fact.** Nothing
  compares numbers at boot, by the argument above; what an installed synod now
  has instead is the guest's own words, via
  [[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]].
- **Media newer than its host** is refused by the same inequality, which is
  correct but says "differ" where it could say "the media is ahead". Whether that
  distinction earns a sentence is unsettled.

## See also

[[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]] (the machine
whose boot this makes checkable),
[[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]
(the same failure, heard rather than prevented),
[[decisions/260727_the-guest-gets-a-network-not-a-verb|the-guest-gets-a-network-not-a-verb]]
(the commit that made the contract 2),
[[decisions/260726_guest-namespace-prefixes|guest-namespace-prefixes]] (the other
fault found by running the installed product rather than by CI),
[[map/synod|synod]] (where the media pipeline and the daemon live).

Cite: `guest-abi/src/boot.rs` (`CONTRACT`, `Boot::read`, `command_line`),
`vm-image/build-tools/src/manifest.rs` (`lines`, `read`, `check`),
`vm-image/build-tools/src/bin/{media-manifest,check-media}.rs`,
`vm-image/build-boot.sh`, `vm-image/out/boot/boot-manifest.txt`,
`synod/tauri.bundle.conf.json`, `vm-image/README.md`.

## Amendment, 2026-10-13

The contract is at 3: the factory port, on which the daemon listens for the host's engine dials ([[decisions/261013_the-seed-rides-the-protocol-sealed|the-seed-rides-the-protocol-sealed]]).

## Amendment, 2026-10-14

The jail's cgroup root is a boot-time agreement outside this contract: the
daemon makes `/sys/fs/cgroup/ral-exec` (`mounts::EXEC_CGROUP`), delegates
`+memory +pids +cpu` into it, and names it to the engine in `RAL_GUEST`; the
engine makes `engine-<pid>/exec-<seq>` and `detached/exec-<seq>` below it. Both ends ship in the same
initramfs, so it cannot skew between host and guest, and `CONTRACT` stays at 3.

## Amendment, 2026-10-27

The daemon names the jail root in argv:
`--guest-engine /sys/fs/cgroup/ral-exec`. This Linux x86-64/aarch64 role
requires one absolute root and euid 0, installs the jail, then serves the
carrier. A role is not inherited by commands; a nested ral therefore boots
as its own shell. The daemon constructs one `Command` for root and factory
engines alike, with a closed environment containing no `RAL_GUEST`.

`PROTOCOL_VERSION` is the engine's whole contract with the host: its frames
and what it refuses its commands. Version 31 requires the guest's seccomp
deny-set, descriptor marking and offline network isolation. Every attach,
development media included, refuses a version mismatch and names both
numbers. The kernel-command-line `CONTRACT` remains at 3; both sides of the
guest argv agreement still ship together.

> **Amended 2026-10-27.** See [[decisions/261027_the-vm-crates-are-layered|the-vm-crates-are-layered]].
> `vm-image/build-tools` owns the manifest: `media-manifest` compiles and prints
> `guest_abi::boot::CONTRACT` and `ral_core::protocol::PROTOCOL_VERSION`;
> `check-media` compares them through `manifest::read` and `manifest::check`.
> `tauri.bundle.conf.json` runs this packaging gate as its `beforeBuildCommand`.
> Present media with an absent or unreadable manifest is refused; absent media
> is left to the bundler.
>
> The std-only `guest-abi` leaf owns `boot::{CONTRACT, Boot::read, command_line}`,
> ports, the virtual link, guest layout and packet framing. Changes to `boot`,
> `ports::FACTORY`, `layout` or `packet` bump `CONTRACT`; this extraction preserves
> version 3. Both backends use `vm-manager/src/spec.rs::boot_line` to call the
> sole command-line writer.
