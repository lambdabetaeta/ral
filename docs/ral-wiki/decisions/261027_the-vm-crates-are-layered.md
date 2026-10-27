---
status: active
verified_at_commit: 3dbbe8f3
verified_at_date: 2026-10-27
anchors: [guest_abi, ral-guest, netgate, synod-machine-broker]
---

# The VM crates are layered

**One ABI leaf every party links (core and synod included); one guest
program with no exec and no argv; the gate owns the ledger and the agent reads one
bit; the privileged service is its own crate.** This decision supersedes nothing.

## Why

Shared facts need one owner, and each layer should require only the authority
it exercises. The previous structure admitted five avoidable defects:

- Host and guest spelled runtime constants independently, allowing drift.
- The broker checked the pipe version after a privileged boot.
- Direct machines requested 6 GiB while broker-owned machines received 4 GiB.
- Host-mode exarch opened an audit ledger it never wrote.
- Exarch and synod applied separate tuning-mask laws.

The ABI leaf, version refusal before boot, shared `SIZE`, gate-owned ledger
and `Tuning::masked` each remove the corresponding second source of truth.

## Decision

- `guest-abi` is std only. The host, guest PID 1, engine, synod and image
  build-tools share the runtime agreement through this leaf.
- `ral-guest` combines assembly and service in one PID 1; its net pump is
  two threads, and its first act refuses execution outside PID 1.
- Core owns egress policy data, `netgate` owns its ledger, and exarch loads
  policy and reads the search bit.
- `vm-manager` owns machines and their lifecycle; `synod-machine-broker`
  owns the privileged service. Synod's guest seam composes their capabilities.

## Dependency invariant

These are the allowed normal dependencies between workspace crates:

| Crate | May depend on |
|---|---|
| `guest-abi` | none (std only) |
| `ral-core` | `guest-abi` |
| `ral-guest` | `guest-abi` |
| `vm-manager` | `guest-abi` |
| `netgate` | `guest-abi`, `ral-core` |
| `exarch` | `ral-core` |
| `ral-vm-image-build-tools` | `guest-abi`, `ral-core` |
| `synod-machine-broker` | `vm-manager` |
| `synod` | `exarch`, `ral-core`, `vm-manager`, `netgate`, `guest-abi` |

Dev-only edges are `vm-manager → ral-core` for boot-smoke,
`netgate → netgate[test-util]`, `synod → {exarch, ral-core}[test-util]`
and `synod-machine-broker → tempfile`.
The removed edges are `guest-net → exarch` and the dependencies on
`ral-daemon` from synod, ral-initramfs, vm-manager, guest-net and build-tools.
`cargo tree -p netgate -e normal` must show no exarch dependency;
`cargo tree -i guest-abi --depth 1 -e normal` checks the leaf's direct users.

## Where

- `guest_abi`: `guest-abi/src/lib.rs` declares `boot`, `ports`, `link`,
  `layout` and `packet`. A change to `boot`, `ports::FACTORY`, `layout` or
  `packet` bumps `boot::CONTRACT`.
- `ral-guest/src/main.rs`: one binary calls `assemble::assemble` then
  `init::serve` in the same PID 1. Admission and shutdown live in
  `init/{fleet,halt}.rs`; `reap::fail` records the first fatal notification.
- `vm-manager/src/spec.rs`: `boot_line` constructs the shared `Boot` and
  calls its sole writer; `machine.rs` owns `Booted` and the required machine
  methods, including `Machine::dial_engine`. `error.rs` owns
  `GuestConsole::report`; the crate root exports the public facade.
- `netgate/src/lib.rs`: the host CONNECT gate exports its session, dialer and
  ledger API; `stack.rs` owns `Config`, `pipe.rs` owns `Wake`, and private
  `refusal.rs` supplies both decoder and vetter. Device access requires
  `test-util`; `ledger.rs` owns records, and core owns egress policy data.
- `synod-machine-broker/src/service.rs`: the privileged service owns each
  connection's machine and pinned folder; `scm.rs` runs its Windows entry points.
  `vm-manager/src/broker/{wire,client}.rs` supply the public protocol and client.
- `synod/src/main.rs`: binary-only composition of `folder`, `wire`,
  `assistant`, `seam` and the desktop-gated `window`. `wire.rs` owns all TS
  types; `assistant/guest.rs` owns the guest seam; `seam/narration.rs` folds
  events through a callback. Generated seam artefacts are untracked.

## Known follow-up

Rootfs-publication eviction and abandoned `.tmp` stage cleanup remain open.
Old cache files and flat session disks are left for the owner to remove.

## See also

[[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]],
[[map/synod|synod]], [[design/egress|egress]].
