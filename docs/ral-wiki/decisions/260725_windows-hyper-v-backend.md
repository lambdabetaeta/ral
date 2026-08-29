---
status: active
---

# Synod's Windows machine is Hyper-V, created directly through HCS

> Amended by
> [[decisions/260725_windows-machine-broker|a `LocalSystem` service holds the privilege, not the user]]:
> the deployment row below — *IT adds the fleet to Hyper-V Administrators* — is
> replaced by a broker service the MSI installs, because that group's members may
> attach a physical disk and so read past every NTFS permission. The machine
> itself is unchanged: the same document, the same devices, the same absent
> network adapter, created by a service instead of by the window. The refusal set
> gains the service's own remedies (not answering, or a version mismatch), and
> the direct path this page describes remains the maintainer's, in a checkout.

> Amended by
> [[decisions/260730_boot-contract-is-versioned|the boot contract has a version]]
> and
> [[decisions/260730_guest-console-outlives-stdout|a service-hosted guest's console is teed to disk]]:
> the first open question below is partly answered, and its two halves are now
> separately owned. The kernel command line the machine carries is one *versioned*
> agreement, compared against the media at package time; and the guest's console
> is no longer only on the host's standard output, which under the broker service
> goes nowhere. Nothing about the document, the devices, or the boot order
> changes.

**Synod boots its own virtual machine on Windows by handing one JSON document to
the Host Compute System API, and every part of that document is chosen so the
guest cannot tell which hypervisor booted it.** `dev/docs/VM/SYNOD.md` §2 named
the route; what this decides is the shape of the machine at the far end of it,
which is where the design record turned out to have promised more ceremony than
the platform needs. Two of its rows are amended by what landed — direct kernel
boot in place of a UEFI machine with a unified kernel image, a fixed VHD in
place of VHDX — and one of its arguments is narrowed: the port does not avoid a
shared filesystem, it uses a different one.

## Context

Synod's premise is topological rather than procedural: the agent is walled off
from the computer not because it is forbidden to leave but because there is
nowhere to go — one folder, one socket, no network device
([[map/synod|synod]], [[design/grant|grant]]). That premise is what decides
every question below, because it rules out any arrangement in which synod's
guest shares a machine with anything else, however convenient the sharing.

Windows offers three ways to reach a hypervisor, and they are not three
spellings of one thing: Hyper-V's WMI provider (the management surface, written
for administrators driving named, persisted virtual machines), the Host Compute
System API (`computecore.dll`, the surface the *Virtual Machine Platform*
feature provides, which WSL 2 and Windows' Linux containers are built on), and
the WSL service itself (a broker over a machine somebody else owns). §2 chose
HCS and named WMI as a fallback to "decide by trying". No trying was needed:
HCS answered, and WMI is not implemented.

## Decision

**HCS directly, not brokered through WSL.** Asking the WSL service for a Linux
environment is the cheapest thing a Windows program can do — it needs no
privilege at all, and the group membership below would evaporate. It is also
the one option synod's premise forbids outright, and not by a margin. A WSL
distribution is a guest in a *shared* utility VM, with `/mnt/c` mounted, with
Windows interop wired into the guest's `binfmt_misc` so a Linux process can
launch a host executable, and with siblings that outlive any one session. Every
sentence of that is the negation of "one folder and nothing else": the wall
would be a filesystem convention inside a machine synod does not own, and the
one door out would be a hole synod cannot see, let alone close. What synod
needs from Windows is not a Linux environment; it is an empty machine.

**The granted folder arrives as a live 9p share the host serves, not as a
session copy over the socket.** §4 describes the workspace as copy-in at
session start and copy-out per accepted file, and §2 leans on that to argue the
Windows port loses nothing by lacking virtiofs. Both backends in fact share the
folder live — virtiofs on macOS, `Devices.Plan9` on Windows — and the reason is
that the copy was never the guarantee. The guarantee is host-side and
independent of transport: a checkpoint of the folder before the job, a
content-addressed history of every byte a job or an undo replaces, a
plain-language change report, and conflict-checked put-back per file or whole
job (`synod/src/workspace/`). Keeping the live share keeps all of that
machinery unchanged and platform-independent, which is worth more than the
copy's incidental isolation; and 9p over a vsock port is not an exotic
mechanism reached for here — it is exactly how WSL serves `/mnt/c` and how
Microsoft's own Linux containers serve every host path. A read-only grant
becomes the *mount's* law (the share carries the read-only flag) rather than a
promise the guest is trusted to keep, which is the same discipline the virtiofs
share holds itself to.

**A fixed VHD, not VHDX.** Hyper-V refuses a raw image, and §2 promised "the
same images wrapped as VHDX at install time". A fixed VHD is the original
format's degenerate case — the disk's sectors verbatim, then one 512-byte
footer describing them — so wrapping is an append: nothing is transcoded, no
block map is built, and the filesystem's bytes stay identically placed, which
is why the guest's kernel reads the device from offset zero and never looks at
the tail. VHDX is the modern format and strictly worse for this job: a
log-structured container with metadata regions and a block allocation table,
which is to say a real converter's worth of code that can be subtly wrong, in
exchange for snapshots, online resize, multi-terabyte disks, and sharing —
every one of which synod declines to use. The wrap happens on first launch
rather than at install time, into synod's own cache, which also means a rebuilt
rootfs is re-wrapped rather than served stale from a marker that only recorded
*that* some wrapping happened.

**Direct kernel boot (`Chipset.LinuxKernelDirect`), not a Gen-2 UEFI machine
with a unified kernel image.** §2's Windows row assumed the Apple shape was
unavailable and budgeted for a bootloader. It is available, and taking it
deletes two things that would otherwise have to be kept in step forever: a boot
disk with an EFI system partition, and a packaging step that fuses kernel,
initramfs, and command line into one signed image. It also collapses a real
divergence between the two backends into none at all — both boot a kernel, an
initial ramdisk, and a command line, and the command line is where every
remaining platform difference is carried (the console device, the workspace
transport). The one asymmetry left is upstream's: an arm64 build has to take
Ubuntu's kernel apart to the raw Image `VZLinuxBootLoader` wants, while an
amd64 `vmlinuz` already *is* the bzImage HCS loads and is kept untouched.

**Every HCS entry point is resolved at runtime, never statically imported.**
`#[link(name = "computecore")]` would be shorter by a page. It would also make
the *process* fail to start on a Windows without the Virtual Machine Platform
feature — before any of synod's code can run, which means before it can say so.
Synod is a desktop application for people who did not choose their own
computer, and running on a machine that cannot host a guest *and explaining
that in one sentence* is part of its Windows story rather than an edge case.
Loading the library on demand turns a missing feature into data, and the same
move covers `computestorage.dll` as a soft dependency, so a Windows that has
one library and not the other is reported precisely rather than as one
undifferentiated absence.

**HCS serves only administrators and members of the local Hyper-V
Administrators group, so whatever process creates the machine must be one of the
two.** The group exists on every Windows and is empty by default. Synod does not
run as administrator, does not ask to be elevated, and does not need any
privilege at run time; it needs the compute service to answer it. That is
checked *before* a folder is granted rather than discovered halfway into a
session, and the refusal names what stood in the way, because a group membership
or a missing feature is what an IT department fixes with one line and a bug
report is what it cannot. Which account carries the requirement is settled by
[[decisions/260725_windows-machine-broker|windows-machine-broker]]: on an
installed synod it is a `LocalSystem` service, and the group is never asked of a
user.

**The control plane's security descriptor names three principals and no
wildcard.** A machine's Hyper-V sockets carry security descriptors, and the
service's default grants `SYSTEM` and the built-in Administrators group —
neither of which synod is, since *Hyper-V Administrators* is a different group.
So the machine's document names this user's own SID alongside those two, and
nothing else. `(A;;FA;;;WD)` — everyone — would have been shorter and would have
let any process on the computer connect to the one door in an otherwise
networkless machine.

## Alternatives considered

- **Hyper-V's WMI provider.** §2's stated fallback, and unnecessary: it is a
  management API for persisted, named virtual machines that appear in Hyper-V
  Manager and survive their creator, where synod wants an unnamed machine that
  dies with the process that made it (which is one line of the document, not a
  lifecycle to police). HCS also carries the two devices synod cannot do
  without — the 9p share and the Hyper-V socket table — as first-class
  document members.
- **Copy-in/copy-out over the control socket**, as §4 describes. It would have
  been the *portable* choice, and it would have bought isolation synod's threat
  model does not need: the workload is the user's own documents, and the failure
  mode is *wrong*, not *hostile*. It costs a bulk plane beside the control
  plane, a manifest protocol, and two full traversals of the folder per
  session; and it would have made the host-side safety net's vocabulary
  transport-dependent. Note that exarch's VM, whose workload really is hostile,
  keeps the opposite position deliberately
  ([[decisions/260715_vm-workspaces-cross-by-copy|vm-workspaces-cross-by-copy]]).
- **A wildcard socket descriptor**, and **`HcsGrantVmAccess` on the user's
  folder**. Both make a class of failure go away by widening authority: the
  first opens the guest's control plane to every process on the computer, the
  second leaves a standing access-control entry for a dead per-VM identity on
  someone's documents. Neither is a trade worth making for a boot that has not
  yet been observed to need it.
- **A worker thread per machine**, as the macOS backend has. Not needed and so
  not built: `VZVirtualMachine` may be touched only from the serial queue it
  was born on, which is a real constraint that a dedicated thread and a message
  discipline answer. An HCS compute system is a handle with no thread affinity
  and no callback queue to host. The only threads in the Windows backend serve
  blocking I/O — the console pump, and the accept that waits for the guest to
  dial.

## Consequences

- **The engine protocol stops being Unix-shaped.** `ral_core::wire::WireStream` is
  `UnixStream` on Unix and `TcpStream` on Windows, and neither name is a claim
  about the address family: under a real guest they own an `AF_VSOCK`
  descriptor and an `AF_HYPERV` socket respectively. Both are legitimate as
  std's owner of a *connected stream socket*, and the borrowing is sound
  exactly because nothing above them ever asks a stream for its own or its
  peer's address. `WireTransport`, its reader and heartbeat threads, and the
  `Transport` impl lose their platform condition; only the same-host engine
  *child* over an inherited socketpair stays Unix
  ([[map/core/transport|core / transport]],
  [[design/engine-protocol|engine-protocol]]).
- **Synod's control seat becomes one function with no platform condition at
  all.** `Machine::take_control` hands back each platform's own owned handle and
  `WireTransport::adopt` takes either, so the seat is assembled the same way on
  both — which is the observable form of "one guest, two lifecycle backends"
  ([[design/engine-protocol|engine-protocol]]).
- **The guest gained one command-line word and lost one hardcoded device.**
  `ral.plan9` names the vsock port the host's 9p server listens on, and its
  presence is the whole of what tells a guest it is under Hyper-V; the
  initramfs probes candidate disk pairs in order, virtio then SCSI, instead of
  naming `/dev/vda`. Neither change is conditional on a hypervisor the guest is
  told about, which is what keeps one rootfs serving both machines.
- **The image pipeline is architecture-parametric**, and the amd64 module set
  is where the platform's sharp edges live: `hv_vmbus` and the drivers on it,
  `vsock` + `hv_sock`, and 9p as *three* modules rather than two, because
  upstream split `trans_fd` into `9pnet_fd` and a mount that finds `9pnet`
  without it fails with a bare `ENODEV`. No `hv_netvsc` is built, because there
  is no network device to drive.
- **A crashed host cannot leave a running guest.**
  `ShouldTerminateOnLastHandleClosed` is one line of the document and it is what
  makes synod's death safe rather than merely tidy; the ordinary path is still
  the clean inside-out halt, where closing the host end of the wire is the
  guest's cue to power itself off.
- **The refusals are enumerated, not generic.** On Windows there are three, each
  a different remedy: the feature is not installed (IT enables a Windows
  feature), this account may not use it (IT adds a group membership), or the
  service is not answering (a fault to report). A synod that cannot put hardware
  between the agent and the computer still refuses to start rather than degrade.

## Open questions

- **The guest's own boot is partly witnessed, and no further.** A machine is
  created and started — through the broker, on an account with no special
  membership — and it boots a kernel and an initramfs that formats the session
  disk and reaches the daemon, which is known because the daemon refused a command
  line carrying a key its own build predated, in its own words, on its own
  console. What is *not* yet witnessed is a guest that completes a boot and dials
  the control plane: the media rebuilt against the host's contract has not been
  booted at the time of writing. `vm-manager/examples/boot-smoke.rs` and
  `synod/examples/boot-run.rs` remain the vehicles.
  [[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]] is
  why that particular skew cannot recur past a build, and
  [[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]
  is why the next failure of any kind will say why on an installed synod and not
  only in a checkout.
- **Whether the host's 9p server can read the granted folder without an
  explicit access grant is untested.** `HcsGrantVmAccess` is called on the four
  boot files, since a virtual-machine worker process really does open those as
  its own virtual account, and deliberately not on the user's folder, for the
  reason given above. If a guest's mount is refused, that is the knob — and the
  fix should be scoped to the session rather than left standing on the folder.

## See also

[[map/synod|synod]] (where the backend lives),
[[decisions/260725_windows-machine-broker|windows-machine-broker]] (who creates
the machine, and why it is not the user),
[[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]] (the
command line as a versioned agreement, checked at package time),
[[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]
(how a boot failure reaches a reader),
[[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]]
(what teardown leaves behind, and the sweep that reclaims it),
[[decisions/260721_synod-is-a-second-product|synod-is-a-second-product]] (the
product this machine serves, and the no-speculative-generality principle applied
here to VHDX and to WMI),
[[decisions/260715_vm-workspaces-cross-by-copy|vm-workspaces-cross-by-copy]] (the
opposite workspace position, for a hostile workload),
[[design/engine-protocol|engine-protocol]] (one engine, one
connection, whichever socket family carries it),
[[map/core/transport|core / transport]] (the framed protocol),
[[design/two-enforcers|two-enforcers]] (the machine is the outer ceiling; ral's
own gate and per-spawn jail stand inside it),
[[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]] (the same
"make the Windows boundary as explicit as the Unix one" discipline, one layer
down),
`dev/docs/VM/SYNOD.md` §2 (the platform table this amends).
