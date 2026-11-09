# Broker-free Windows execution

Date: 2026-11-09

Status: proposed; implementation and native validation are not authorised by this document.

## Objective

Replace Synod's Windows HCS backend with a user-owned OpenVMM process and delete
`synod-machine-broker`. Preserve the small `vm-manager` application boundary and
netgate's exclusive ownership of guest egress. Leave macOS on
Virtualization.framework.

Success means a standard desktop user, outside Administrators and Hyper-V
Administrators, can run a complete Synod session without elevation or a custom
privileged service. Merely booting Linux, or moving our broker policy into a
third-party SYSTEM daemon, does not meet the objective.

Administrator-approved installation may enable Windows Hypervisor Platform
(`HypervisorPlatform`) and require a reboot. Installation authority must not
become runtime authority.

## Evidence and scope

The feasibility investigation inspected RAL at
`e2e104f8c58c9377379e36123855c06705bcff2d` and OpenVMM at
`ba105dd7f00499acc1862456c179c0930cb2c8f7`. It ran on Linux x86_64; no native
Windows operation has been demonstrated.

Source evidence supports WHP partition creation, raw file disks, Windows
virtiofs, and bidirectional VMBus socket relaying. It also establishes that:

- OpenVMM's dynamic VHD path calls `AttachVirtualDisk`, whose documented
  privilege requirement defeats an unchanged-storage migration.
- OpenVMM's built-in virtiofs and virtio-9p host implementations exclude macOS.
- Our Windows `connect_guest` implementations currently return `Unsupported`;
  the factory connection is a required capability to implement, not existing
  functionality to assume.
- OpenVMM's ttrpc virtiofs configuration exposes only tag, root and read-only;
  it does not expose the Windows Linux-metadata option requested by today's HCS
  share. Resolve whether this prevents the required filesystem semantics before
  investing in a native probe; do not treat it as an unknown configuration flag.
- OpenVMM source releases do not supply a turnkey signed desktop runtime or a
  servicing commitment. We would own binary production and updates.

Initial target: native Windows 11 x64 on Intel and AMD. Record the actual minimum
Windows build and hardware requirements before declaring support. Windows ARM64,
Windows 10, Linux host support, and a macOS migration are outside this plan.
Guest media may change deliberately; there is no compatibility requirement for
HCS-specific artifacts or the old broker protocol.

## Intended architecture

```text
Synod (desktop user's token)
  vm-manager Windows adapter
    private OpenVMM management client
    one supervised OpenVMM process tree per Machine
      WHP partition
      read-only rootfs + disposable session file
      one workspace export
      control / net / engine-factory socket relay
```

Keep `Hypervisor::boot`, `Booted`, `Wires`, and `Machine` as the application
boundary (`vm-manager/src/machine.rs`). Preserve owned Windows sockets, not a
new application-facing RPC abstraction. Product policy and engine orchestration
remain in Synod; guest egress policy remains in netgate.

Use a pinned OpenVMM executable and its private ttrpc VM service. Do not embed
unpublished upstream workspace internals, automate the REPL, or introduce a
shared multi-user daemon. Pin the executable and protocol schema together.
Confirm this management path can express the entire configuration in Gate 2;
stop rather than maintain a second control path if it cannot.

### Exact guest topology

| Resource | Choice |
|---|---|
| Boot | Existing amd64 Linux kernel and rebuilt initramfs; direct boot with the shared `ral.*` writer |
| CPUs / memory | Existing `vm_manager::SIZE` |
| Storage bus | Emulated Hyper-V SCSI, retaining `hv_storvsc` |
| Root disk | Ordinary raw ext4 file, read-only, first SCSI LUN |
| Session disk | New sparse raw file, writable, second SCSI LUN; retain existing session capacity |
| Workspace | One virtiofs export tagged `workspace`, with host-enforced grant mode |
| Workspace bus | Explicit OpenVMM software VPCI transport; provision the corresponding Hyper-V PCI and virtio drivers |
| Guest sockets | VMBus/hv_sock relay: control 1729, net 1730, factory 1731 |
| Console | Existing serial console spelling, captured by the adapter |
| External NICs | None |

Do not use dynamic VHD attachment, raw host disks, device passthrough, explicit
privileged hugepages, host network adapters, debuggers, or guest-memory dumps.
Do not change device topology silently if this one proves unavailable.

## Ordered gates

Each gate produces a short evidence record: exact host build/CPU, effective user
and group membership, OpenVMM pin, media identities, configuration, observations,
and unresolved failures. A gate passes only for demonstrated behaviour, not a
CLI flag or an upstream capability list. Stop at a failed gate and report the
specific discrepancy; do not compensate with elevation or a custom server.

### Gate 1 — ordinary-user execution and deployable dependency

1. Identify a reproducible source build, required native dependencies, licence
   notices, signing requirements, and supported Windows floor for the pinned
   executable. Establish who owns upstream security updates.
2. Provision WHP separately through an administrator. Use a standard account
   outside both administrator groups for all subsequent VM operations.
3. Demonstrate WHP creation and execution with ordinary file-backed resources,
   with no Synod broker installed or running. Exclude accidental reliance on an
   elevated terminal, volume-management privilege, or another product's daemon.
4. Inspect effective configuration and permissions. Record any denied operation
   precisely before deciding whether the architecture is viable.

**Exit:** the basic runtime is service-free and unprivileged, and its build and
redistribution path is understood. If that requires privileged mediation, stop:
this plan has not achieved its objective.

### Gate 2 — complete guest and filesystem contract

Use a disposable probe, outside the production backend, with the exact topology
above and private ttrpc control. Verify `CreateVM` creates a paused VM, configure
all resources, then use `ResumeVM`; process readiness is not guest readiness.

- Add virtiofs, virtio transport, and Hyper-V PCI modules/dependencies to the
  amd64 branch of `vm-image/build-boot.sh`. Verify the generated kernel config
  and module manifest; do not assume the downloaded kernel contains them.
- Select the existing Virtiofs boot variant rather than 9p-over-vsock. Keep PID1
  and the engine protocol unchanged unless an actual discrepancy requires a
  separately approved change. Do not bump the boot ABI merely for rebuilding
  drivers; bump it if the shared agreement itself changes.
- Establish root/session identity and ordering before allowing PID1 to format
  the second disk. The granted folder must never be a candidate block device.
- Establish workspace read/write behaviour, ordinary Windows ACL enforcement,
  Linux metadata behaviour, Unicode/case handling, and relevant supported
  filesystem limitations. Resolve the known ttrpc metadata-option gap against
  the actual required semantics first. If meeting them requires an unsupported
  management path or a private upstream fork, stop and report rather than
  silently weakening the contract.
- Establish host-side read-only enforcement against write, truncate, rename,
  unlink, link and metadata mutations, including guest remount attempts.
- Establish granted-root identity and confinement under rename, symlink,
  junction/reparse-point and descendant-access scenarios. User ownership limits
  authority to that user's token; it does not confine the VMM to one folder.
  Describe existing hardlink/mount behaviour explicitly rather than claiming a
  general filesystem sandbox. Do not add a bespoke filesystem server.
- Demonstrate both guest-initiated wires and repeated host-initiated factory
  dials, including the guest's host-CID admission check. Consume the hybrid
  `CONNECT`/`OK` handshake with bounded framing, preserving all following engine
  bytes. Verify EOF, half-close and backpressure semantics.
- Establish private endpoint access controls, cross-user exclusion and the
  accepted same-user threat boundary. No guest-selectable relay destination may
  reach an unintended host service. Verify no NIC or alternate egress path is
  present; use the unchanged net wire and netgate.

**Exit:** a complete guest session works without HCS, elevation, a new filesystem
server, or weakened folder/egress guarantees. Record any unsupported folder type
or filesystem as an explicit product refusal, not an implicit fallback.

### Gate 3 — ownership, shutdown and failure containment

Design one owner for the complete machine lifetime:

1. Create an owner-restricted per-session runtime directory and prebind the two
   guest-wire listeners before guest execution. Preserve grant identity through
   host share opening; do not replace authorisation with canonicalisation alone.
2. Create a Windows kill-on-close job. Start OpenVMM suspended, assign it before
   resuming, and establish containment of its workers/nested jobs. No descendant
   may inherit the handle whose closure is supposed to terminate the tree or
   escape through breakaway. Do not rely on parent PID or a pidfile.
3. Launch only the exact installed executable with trusted arguments and explicit
   handle inheritance. Control DLL search, environment and working directory;
   do not discover a helper through PATH or the granted folder. Keep management
   endpoints private; do not expose general OpenVMM configuration to clients.
4. Use one bounded boot transaction covering process/RPC readiness, VM creation,
   resume and both accepted wires. Preserve the existing shared 60-second guest
   readiness ceiling; bound connection, handshake and management operations too.
   Return `Booted` only after both wires are owned successfully.
5. On partial failure, stop the tree, reap it, preserve bounded console evidence,
   then release session resources. Resource-owning types must enforce this same
   order on early return and Drop; avoid parallel flags describing machine
   liveness. No transparent restart or reattachment to stale wires.
6. Prove a cooperative shutdown request reaches the existing PID1. An IC
   acknowledgement is not proof of guest poweroff; do not assume ACPI button
   handling or `force=true` is host termination. Retain the existing bounded
   grace followed by job termination and process reaping when necessary.
7. Exercise failed/no-ready boot, busy shutdown, guest poweroff, VMM/worker
   failure, app termination and simultaneous sessions. Establish that the entire
   tree stops even when Synod cannot run cleanup code. Upstream documents worker
   teardown deadlock risks, so never await teardown without a deadline.
8. Delete session files only after process exit. Retain lease-based recovery for
   abandoned disks and owned endpoint/log directories; forced app death cannot
   be expected to delete files synchronously. Never sweep another live session
   or use a reusable PID as proof of ownership.

**Exit:** normal and abnormal exits leave no running guest/workers, no reachable
management endpoints, and no indefinitely held disks. Crash leftovers are safely
reclaimed by the next owner; failed-boot diagnostics remain useful and bounded.

### Gate 4 — production adapter and packaging

After Gates 1–3, implement only the established mechanism in
`vm-manager/src/` as a Windows-specific OpenVMM backend. Reuse platform-neutral
media verification, resource sizing, session leases and diagnostic types;
remove broker-only serialization/errors when no longer needed. Keep private
configuration types restricted to the chosen topology; no generic runtime
registry, backend-selection setting, hotplug API or compatibility fallback.

Package the pinned executable, required native dependencies, notices and
provenance with Synod. Keep executable/schema/media versions coherent, avoid
user-writable dependency search paths, and establish signed installation,
upgrades, uninstall and missing-WHP diagnostics. Machine-wide installation does
not justify a machine-wide runtime cache: use per-user storage and ownership.

The packaged app must pass the same standard-user and lifetime gates as the
probe. A developer build launched from an administrator shell is not acceptance.
Leave the macOS executable, entitlements, guest recipe and VZ path unchanged.

**Exit:** a distributable Windows app runs a real Synod session through the new
backend, including child-engine factory use, without a privileged helper.

### Gate 5 — delete the privileged path

Only after the preceding gates, switch Windows detection directly to the new
backend and remove, rather than retain as a fallback:

- `synod-machine-broker/` and its workspace/lockfile entries.
- `vm-manager/src/broker/`, `vm-manager/src/hcs/`, and `broker-smoke`.
- `synod/wix/broker-service.wxs` and its Windows bundle references.
- `_broker`, `broker-install`, and `broker-uninstall` recipes in
  `synod/mod.just`.
- HCS/broker-specific dependency features, error variants, media lookup branches,
  service-cache assumptions, and build/release references after a repository-wide
  reference audit.
- The obsolete guest 9p-over-vsock export variant, its port and amd64 driver
  entries if the reference audit confirms no remaining consumers. Reflect any
  resulting boot-agreement change in the shared ABI contract and rebuilt media.

Replace misleading Windows privilege/device-model documentation; retain the
macOS design. Document the Windows floor, install-time WHP prerequisite,
per-user runtime ownership, raw disks, virtiofs export and factory support.
Use `git rm` for tracked removals and `git mv` for tracked moves. Do not preserve
legacy broker IPC, HCS media formats or a dual-backend rollout mechanism.
There is no requirement to upgrade old service installations in this research
migration; use a clean installation for acceptance. Record that existing service
installations require explicit administrator removal before use, rather than
silently leaving an old LocalSystem service installed.

**Exit:** no Synod service is installed, started, detected or contacted; no HCS
fallback is reachable. Both direct Windows use and the distributed application
satisfy the objective. The macOS backend remains unchanged.

## Work order and validation authority

Writing this plan authorises none of its implementation, builds, native VM
exercises or packaging changes. The current project instruction permits lints
only, not tests or CI. Obtain explicit permission for the native acceptance
exercises and artifact builds before executing them; do not disguise those as
linting. Until native Windows hardware/accounts and that permission are
available, the gates remain unverified and production deletion is blocked.

Proceed in gate order. Do not build the production adapter while fundamental
privilege or filesystem questions remain unresolved. A discrepancy warrants a
short report and a decision, not an improvised privileged service, filesystem
server, management API fork, or weaker contract.

## Evidence anchors

Local ownership and integration:

- `vm-manager/src/{lib,machine,spec,lifecycle,media}.rs`
- `vm-manager/src/hcs/{mod,spec,vhd}.rs`
- `vm-manager/src/broker/client.rs`
- `synod-machine-broker/src/{service,scm}.rs`
- `ral-guest/src/{mount,vsock}.rs` and `ral-guest/src/assemble/plan.rs`
- `vm-image/build-boot.sh`
- `synod/src/assistant/guest.rs`, `synod/mod.just`,
  `synod/tauri.windows.conf.json`, `synod/tauri.bundle.conf.json`

Pinned upstream and platform references:

- [WHP backend](https://github.com/microsoft/openvmm/blob/ba105dd7f00499acc1862456c179c0930cb2c8f7/openvmm/openvmm_hypervisors/src/whp.rs)
- [Disk opening and dynamic-VHD dispatch](https://github.com/microsoft/openvmm/blob/ba105dd7f00499acc1862456c179c0930cb2c8f7/openvmm/openvmm_helpers/src/disk.rs)
- [AttachVirtualDisk privilege requirements](https://learn.microsoft.com/en-us/windows/win32/api/virtdisk/nf-virtdisk-attachvirtualdisk#remarks)
- [VM management schema](https://github.com/microsoft/openvmm/blob/ba105dd7f00499acc1862456c179c0930cb2c8f7/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto)
- [Filesystem constraints and Windows semantics](https://github.com/microsoft/openvmm/blob/ba105dd7f00499acc1862456c179c0930cb2c8f7/vm/devices/support/fs/lxutil/src/lib.rs)
- [Platform device registration](https://github.com/microsoft/openvmm/blob/ba105dd7f00499acc1862456c179c0930cb2c8f7/openvmm/openvmm_resources/src/lib.rs)
- [Source release scope](https://github.com/microsoft/openvmm/blob/ba105dd7f00499acc1862456c179c0930cb2c8f7/Guide/src/dev_guide/contrib/openvmm_source_release.md)
