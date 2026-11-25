---
status: active
supersedes: decisions/260725_windows-hyper-v-backend, decisions/260725_windows-machine-broker
generated_at_commit: 723d5391
verified_at_commit: 723d5391
verified_at_date: 2026-11-24
anchors: [OpenVmm, SessionDisk, SESSION_BYTES, HypervisorPlatformOff, NoVmm, SocketPathTooLong, NoPlainSpelling, EndedBeforeDial, Unanswered, create_vm, EXIT_RESET]
---

# Synod's Windows machine is OpenVMM on WHP, run as the user

**On Windows, Synod boots its guest in an unprivileged `openvmm.exe` that it
spawns under the user's own token, on the Windows Hypervisor Platform (WHP).
There is no Host Compute System (HCS) call, no `LocalSystem` service, no
elevation and no VHD.** The machine is the one macOS already boots: a raw
ext4 rootfs, a sparse raw session disk, the granted folder over virtio-fs,
vsock for every wire, and no network adapter. Exactly one process holds the
machine, and the process is the machine's lifetime.

Supersedes
[[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]] (HCS,
Plan 9, VHD) and
[[decisions/260725_windows-machine-broker|windows-machine-broker]] (the
`SynodMachineBroker` service). Their premise stands: one folder and nothing
else, and an empty machine rather than WSL's shared one.

## Context

HCS serves only administrators and Hyper-V Administrators. Joining that group
lets a user attach a physical disk and read beneath NTFS, so the broker ADR
moved the privilege into a `LocalSystem` service instead. That service was
the part of Synod most in need of review. It needed impersonation, a pinned
folder, socket duplication across processes, a versioned pipe protocol, a
machine-wide cache and a WiX fragment. Windows also had no host-to-guest dial,
so helper engines (`ports::FACTORY`) were unsupported there.

WHP is the hypervisor API that any user may call once the *Windows Hypervisor
Platform* feature is on. OpenVMM (Microsoft, MIT) is a user-mode VMM over WHP
that speaks VMBus storage and sockets, and virtio-fs over VPCI. Together they
need no privilege at run time.

## Decision

- **One child process, OpenVMM, pinned and staged beside Synod.**
  - **Version:** OpenVMM is built from a pinned upstream commit with only the
    WHP backend (`scripts/build-openvmm.ps1`).
  - **Staging:** `openvmm.exe` ships beside `synod.exe` as a Tauri bundle
    resource, with its licence and `THIRDPARTY-OPENVMM` under `licenses\`. A checkout stages it in
    `target/openvmm/`.
  - **Resolution:** `openvmm::available()` looks beside `current_exe()`, then
    in the checkout. It never searches `PATH` or the granted folder, and it
    spawns by absolute path.
  - **No new crates:** `vm-manager` gains no third-party crate. The ttrpc
    framing and the protobuf encoding of `CreateVM` are hand-written in
    `openvmm/{ttrpc,proto}.rs`, with golden wire tests that run on Linux.
- **Refusals before boot, in order.**
  - **WHP off:** `WinHvPlatform.dll` is loaded from System32 only, and
    `WHvGetCapability(HypervisorPresent)` is asked. A missing DLL, a missing
    entry point or `false` gives `Error::HypervisorPlatformOff`. Its text names
    the feature, the `Enable-WindowsOptionalFeature` line and the restart.
    The installer never toggles Windows features.
  - **No VMM:** a missing `openvmm.exe` gives `Error::NoVmm { looked_in }`,
    like `NoGuestImage`.
  - **No fallback:** there is no second backend to fall back to.
- **Raw disks, no VHD.**
  - **Disks:** both are on one VMBus SCSI controller. LUN 0 is the shared raw
    ext4 `rootfs.img`, read-only. LUN 1 is `session.img`, writable.
  - **Session disk:** `lifecycle::SESSION_BYTES` (8 GiB) is shared with VZ.
    On Windows, `FSCTL_SET_SPARSE` precedes `set_len`, since NTFS otherwise
    zeroes 8 GiB eagerly. A volume that cannot mark a file sparse is refused
    as `HostFile::SessionDisk`.
  - **Boot:** the kernel boots directly (`HyperVGen2LinuxDirect`, no UEFI),
    so both platforms boot a kernel, an initramfs and a command line.
- **The folder is VPCI virtio-fs with Linux metadata.**
  - **Mount options:** the tag is the boot command line's `ral.workspace`.
    The options are `metadata` for a writable grant and `metadata;ro` for a
    read-only one, which virtio-fs enforces with `EROFS`.
  - **Paths keep their meaning:** OpenVMM is given every path in Windows'
    ordinary spelling, and so are the sockets, the executable and the working
    directory. `win::plain` strips `\\?\` and refuses with
    `Error::NoPlainSpelling` unless `GetFullPathNameW` reads that spelling
    back as itself. A name ending in a dot or a space, or a device name, would
    otherwise send the guest a different folder from the one granted. The
    temporary folder is checked once, since every session path is a plain
    name beneath it; its refusal, `TempNoPlainSpelling`, says to move it.
  - **Contract 4:** `guest_abi::boot::Export`, `ral.plan9` and
    `ports::PLAN9` are gone. `Boot.workspace` is a tag, and `CONTRACT` is 4
    ([[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]]).
  - **amd64 kernel:** gains `pci-hyperv`, `virtio_pci`, `virtiofs` and
    `fuse`, and drops 9p.
  - **NTFS dependence:** Linux metadata (modes, ownership, symlinks) is kept
    by OpenVMM's `lxutil`, which probes the host filesystem for the features
    it needs. A workspace on FAT, ReFS or a network share may lose metadata.
    NTFS is the supported case, and acceptance runs only there.
- **Every wire is OpenVMM's hybrid vsock relay over `AF_UNIX`.**
  - **The socket:** OpenVMM binds `v.sock` in the session directory.
  - **Guest to host:** before `ResumeVM`, Synod binds `v.sock_1729`
    (control) and `v.sock_1730` (net). A guest connection to port *p* is
    relayed, raw, to `v.sock_<p>`. OpenVMM sees a listener only by whether
    the file exists, so a stale file would fail after the guest has opened
    the channel; the session directory is fresh every boot.
  - **Host to guest (`connect_guest`, `dial_engine`):**
    1. Synod connects to `v.sock` and writes `CONNECT <port>\n`.
    2. It sends nothing more until `OK <id>\n` arrives, reading at most 64
       bytes, one at a time, within `GUEST_DIAL_PATIENCE`.
    3. End-of-file without `OK` is `DialError::Unanswered { port }`.

    Helper engines therefore work on Windows, and `DialBackend::OpenVmm` names
    the backend. Every backend can now dial the guest, so
    `DialError::Unsupported` and its fact are gone. The engine's fact
    vocabulary changed with it (`EngineFactoryFailure` gains
    `dial-unanswered` and loses `unsupported`; `FactoryBackend` is
    `open-vmm` or `virtualization`), so `PROTOCOL_VERSION` is 50.
  - **The engine protocol's stream:** on Windows `WireStream` is std's
    `TcpStream` owning an `AF_UNIX` socket. As before, the type makes no
    claim about the address family
    ([[design/engine-protocol|engine-protocol]]).
- **The Job Object is the machine's lifetime, and power events end the
  process.**
  - **The job:** an anonymous, non-inheritable Job Object with
    `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and no breakaway. The child is
    placed in it at creation (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), so it never
    runs outside it, even if Synod dies between two calls. It inherits
    exactly three handles (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`): `NUL` for
    stdin and stdout, and the openvmm log for stderr. Its environment holds
    only `SystemRoot`, `TEMP` and `TMP`, and its working directory is the
    session directory. A Synod that dies, however it dies, takes OpenVMM with
    it.
  - **Readiness:** Synod tries to connect to `r.sock` every `PULSE` (50 ms),
    waiting on the process handle in between. The first connection that
    succeeds is the RPC stream; the session directory is fresh, so an
    accepting `r.sock` is OpenVMM's. An exit first is refused with its status
    and the openvmm log's tail, and `BOOT_TIMEOUT` with the last connect
    error. This departs from the plan, which read readiness from OpenVMM
    closing stdout: a process spawned elsewhere in Synod at the same moment
    can inherit an inheritable pipe end and hold that end-of-file back.
  - **RPC:** the RPC surface is `CreateVM` and `ResumeVM`, nothing else.
    OpenVMM runs VM RPCs one at a time and ignores their deadlines, so a
    teardown made of RPCs could hang behind one stuck call. A process handle
    cannot. An I/O failure or timeout on either call is reported with
    OpenVMM's exit status when it exits within two seconds.
  - **Killing:** `TerminateJobObject` is checked, and the reap waits at most
    two seconds; dropping `Vmm` does both, and closing the job handle is the
    backstop. A forced
    stop that fails is OpenVMM's failure (`VmmFailed`), not `ForcedStop`.
  - **Power events:** `GuestPowerActions` turns every guest power event into
    process exit, as follows. Any other code is OpenVMM's own failure.

    | Event | Exit code | Meaning to `stop_within` |
    |---|---|---|
    | shutdown | `EXIT_SHUTDOWN` = 0 | stopped |
    | reset | `EXIT_RESET` = 64 | an error naming the event |
    | crash | `EXIT_CRASH` = 65 | an error naming the event |
    | watchdog | `EXIT_WATCHDOG` = 66 | an error naming the event |

  - **Stopping:** the guest stops from inside. Every caller closes the wires
    first, which starts PID 1's `halt()`. `stop_within` then waits for the
    exit code and, past `STOP_GRACE`, terminates the job (`ForcedStop`).
  - **Before the wires:** a power event before both wires are dialled is
    `Error::EndedBeforeDial`, carrying the `PowerEvent`. A boot that simply
    never dials is `Undialled`, whose likely cause is chosen per wire. Both
    quote the console and name the openvmm log, since OpenVMM can drop a
    failed VM worker while its RPC process lives on.
- **One session directory, with a 107-byte `AF_UNIX` budget.**
  - **Contents:** `SessionDisk::create` makes
    `<temp>\ral-vm-session-<epoch>-<pid>-<rand>\` on both platforms. It holds
    `lease`, `session.img`, `r.sock`, `v.sock` and `v.sock_<port>`.
  - **Private from creation:** on Windows `CreateDirectoryW` is given a
    protected DACL (SYSTEM, Administrators, the owner), so no handle can be
    opened before it holds; on Unix the directory is made with mode `0700`.
  - **Path budget:** Synod binds and connects only to socket paths of at most
    107 UTF-8 bytes. A longer path is `Error::SocketPathTooLong { path }`,
    whose text names the limit and the remedy: point both `TMP` (which
    Windows reads first) and `TEMP` at a short folder of the user's own on
    NTFS. The sparse-volume refusal gives the same remedy. OpenVMM's own
    paths are relative to its working directory, so they never meet the
    limit.
  - **Cleanup:** one predicate recognises our socket entries: the names
    `r.sock`, `v.sock` and `v.sock_<digits>`, each an
    `IO_REPARSE_TAG_AF_UNIX` reparse point. `SessionDisk`'s `Drop` and the
    start-up sweep both use it, so a clean stop leaves nothing and a crashed
    one is reclaimable.
- **The console is a named pipe that Synod serves and OpenVMM opens.**
  - **The pipe:** it is created before `CreateVM`, with an owner-only DACL
    and `PIPE_REJECT_REMOTE_CLIENTS`. Dropping a `Console` wakes its pump, so
    no refusal leaks one.
  - **The logs:** the console log and `synod-openvmm-<id>.log`, OpenVMM's
    stderr, live in the user's own `%LOCALAPPDATA%\Synod\Machine`. The
    machine exists only once both wires are dialled, so a failed boot keeps
    both logs by construction; only a clean stop discards them, since a
    refused stop may name either. Both are swept by the console log's sweep
    ([[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]).

## The accepted trade-off

OpenVMM's device emulation (virtio-fs, storvsp, VMBus) runs in a process that
carries the user's full token.

- **Under HCS**, the worker ran as a per-VM virtual account that could reach
  only what was granted to it.
- **Under VZ**, the machine runs in Apple's sandboxed XPC service.
- **Here**, an escape from the guest into OpenVMM reaches every file the user
  can, which is weaker than "one folder and nothing else" at the host
  boundary.

What this buys is the absence of any privileged component. The broker's
physical-disk escalation is gone, because a user token cannot open
`\\.\PhysicalDrive*`, and there is no `SYSTEM` service left to review.

**Follow-up:** confine `openvmm.exe` with a restricted token, low integrity
or an AppContainer, so that its reach is the granted folder, the media and
the session directory. This is not yet done.

## Alternatives considered

- **Keep HCS behind the broker.** It is correct, but a privileged service is
  the most expensive thing Synod ships to review, and it still had no
  host-to-guest dial.
- **HCS with the user in Hyper-V Administrators.** Rejected by the broker ADR
  for the physical-disk escalation, and that reason still holds.
- **Teardown by RPC (`ShutdownVM`, `TeardownVM`, `Quit`).** OpenVMM
  serialises these calls and ignores their deadlines. The exit code says the
  same thing and cannot hang.
- **A shutdown integration-component request, or a guest `poweroff` shim.**
  Both race the inside-out halt that every caller has already started. The
  shim also needed an external `kill` and a shell script. PID 1's `halt()`
  already powers off, and blocked signals are never dropped for it.
- **A separate runtime directory for sockets.** No sweep could reclaim it,
  because it had no epoch in its name and its sockets are reparse points. One
  session directory, with one predicate, is reclaimable.

## Consequences

- **Removed:** `vm-manager/src/{hcs,broker}/`, `os_path.rs`, the
  `synod-machine-broker` crate, `synod/wix/`, the broker `just` recipes,
  `serde`/`serde_json` in `vm-manager`, the VHD wrap, the machine-wide
  `%ProgramData%` cache, `HcsGrantVmAccess`, and every broker refusal
  (`ServiceUnlocated`, `VersionSkew`, `NotPermitted`, `Folder*`).
- **`NoBackend`** now names WHP.
- **The installer** carries `openvmm.exe` as a resource and registers nothing.
  `NOTICE` and `packaging/README.md` record OpenVMM's MIT licence, and the
  build generates `THIRDPARTY-OPENVMM` with cargo-about from the exact graph
  it compiles, adding the in-source attributions cargo-about cannot see
  (Firecracker and Chromium OS in the ELF loader, `fuse.h`, Google's RPC
  protos).
- **Memory:** guest RAM is a pagefile-backed section committed at boot. The
  working set grows lazily, but the commit charge is the full `SIZE` (6 GiB)
  from the start.
- **macOS changes only through shared code:** `CONTRACT` 4, which forces an
  arm64 media rebuild, plus `SESSION_BYTES` and the shared `lifecycle` and
  `error` edits.
- **The public boundary is unchanged:** `Hypervisor::boot`, `Booted`,
  `Wires`, `Socket` and `Machine`. `SIZE` stays 4 vCPUs and 6144 MiB, with
  the vCPUs clamped to host parallelism.

## Where

- `vm-manager/src/openvmm/mod.rs` declares the modules: `proto`, `relay`
  and `ttrpc` compile wherever `openvmm` does, so their tests run on Linux;
  the rest is Windows-only.
- `vm-manager/src/openvmm/hypervisor.rs` has `OpenVmm`, `available` and the
  guest.
- `vm-manager/src/openvmm/proto.rs` has `create_vm`, `Machine`, `Disk`,
  `Share`, the `EXIT_*` codes and `exit`, their meaning.
- `vm-manager/src/openvmm/relay.rs` has the `CONNECT`/`OK` handshake.
- `vm-manager/src/openvmm/ttrpc.rs` has `Client`, `Method` and `Failure`.
- `vm-manager/src/openvmm/process.rs` has the job, the spawn, readiness and
  `kill`.
- `vm-manager/src/openvmm/afunix.rs` has Winsock `AF_UNIX` and the budget.
- `vm-manager/src/openvmm/console.rs` has the console pipe and logs.
- `vm-manager/src/openvmm/win.rs` has `plain`, `document_text`, sparse
  creation and `private_directory`.
- `vm-manager/src/lifecycle.rs` has `SessionDisk`, `SESSION_BYTES`, the
  socket predicate and the sweep.
- `vm-manager/src/dial.rs` has `DialBackend::OpenVmm` and
  `DialError::Unanswered`.
- `vm-manager/src/error.rs` has `HypervisorPlatformOff`, `NoVmm`,
  `SocketPathTooLong`, `NoPlainSpelling`, `EndedBeforeDial`, `VmmFailed`
  and `PowerEvent`.
- `guest-abi/src/boot.rs` has `CONTRACT` and `Boot.workspace`.
- Staging: `scripts/build-openvmm.ps1`,
  `synod/tauri.bundle.windows.conf.json` and `packaging/about/`.

## See also

[[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]] and
[[decisions/260725_windows-machine-broker|windows-machine-broker]] (what this
replaces, and the premise it keeps),
[[decisions/261027_the-vm-crates-are-layered|the-vm-crates-are-layered]],
[[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]],
[[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]],
[[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]],
[[design/engine-protocol|engine-protocol]], [[design/two-enforcers|two-enforcers]]
(the machine is the outer ceiling), [[map/synod|synod]].
