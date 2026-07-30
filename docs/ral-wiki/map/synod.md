---
generated_at_commit: 7c1c8ae6
generated_at_date: 2026-07-30
covers_paths: [synod/, vm-manager/, ral-daemon/, ral-initramfs/, vm-image/, core/src/wire.rs, core/src/transport.rs]
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
It is one crate in two halves: the library modules are the engine anyone
could drive, and the desktop shell rooted at `synod/src/main.rs` is the only
thing that drives it.

The design record is `dev/docs/VM/SYNOD.md`; the landed state is
`dev/docs/VM/SYNOD-v1.md`. Every conversation boots a real hardware machine
from shipped boot media — there is no software-only fallback — and the engine
runs inside the guest, one engine process per session, driven over the
design's §3 wire
([[decisions/260722_session-is-a-process|session-is-a-process]]). **One guest,
two lifecycle backends**: Virtualization.framework on macOS arm64, Hyper-V
through the Host Compute System API on Windows x86_64
([[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]]). The
two are the same machine assembled from the parts each platform has, and the
guest cannot tell which one booted it, because every difference is either
invisible from inside (the disk bus, the socket family) or carried on the
kernel command line. On Windows the machine is created by a `LocalSystem`
service the installer registers, asked for over a named pipe by an
unprivileged window, so nobody has to be given a hypervisor's privilege to run
synod ([[decisions/260725_windows-machine-broker|windows-machine-broker]]).

## synod/ — the library

- `lib.rs` — the crate doc names the five differences from exarch.
- `build.rs` — the Tauri build, plus the one thing the bundle cannot check for
  itself: `boot_contract` reads `vm-image/out/boot/boot-manifest.txt` at the
  resource map's own path and puts it to `ral_daemon::boot::check_media`, so
  media whose boot contract is not this host's fails the *build* rather than
  the guest's own reading of its command line
  ([[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]]).
  `ral-daemon` is a build dependency for exactly this; absent media is not a
  failure here, since the bundle is where Tauri names a missing resource.
- `boot.rs` — the media this build ships, found and readied once.
  `boot_media()` looks in three places, each simply *a place a file might be*
  rather than a `#[cfg]` branch: a macOS bundle's `Contents/Resources/boot/`,
  a Windows installation's `boot/` beside the executable, then the development
  pipeline's `vm-image/out/`. `BootPlan::realise` inflates a shipped zstd
  rootfs into the XDG cache against its `sha256` sidecar — a signed bundle is
  read-only, so the cache is the only writable home the image has — and yields
  a `vm_manager::BootArtifact`. What `session::begin` hands
  `vm_manager::detect` is a `vm_manager::BootMedia` closure over `realise`,
  not the artifact itself.
- `grant.rs` — the folder becomes a `ral_core::types::Capabilities`
  ([[design/grant|grant]]), and a `vm_manager::MachineSpec` naming the same
  folder. One grant, read twice: once as authority, once as a workspace.
- `prompt.rs` + `data/*.md` — the office persona and the office toolbox,
  assembled through exarch's own section renderer (`exarch::prompt::render`)
  and grant rendering (`exarch::prompt::grant_summary`), over a `host_section`
  of synod's own that tells the agent guest truths only.
- `session.rs` — `Conversation`, one folder held open from first message to
  last. `begin` opens the grant, boots the machine, and seats exarch's agent
  on the wire the machine hands back (`exarch::agent::RootSeat::Wire` over
  `Machine::take_wires`, attached at the guest's `/work`). `control_seat`
  carries no platform condition at all: `take_wires` hands back each
  platform's own owned handles and `ral_core::transport::WireTransport::adopt`
  takes either, so the seam is one function
  ([[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]]).
  `exchange` drives one message through `exarch::headless::converse_sink`,
  bracketed by the safety net — a checkpoint before, a checkpoint after, even after a
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

## synod/src/shell/ — the window

The desktop shell (Tauri v2, hand-written static frontend, no bundler, pure
cargo) is the one process: it holds the `Conversation` in-process — no child
binary, no stdin framing. One window, three states: choose a folder and a
model (with a Thinking control beside the Assistant picker) and describe the
job; watch the assistant work, its narration streamed in; then read what
changed and put anything back. `commands.rs` holds the folder picker, the
conversation verbs (start, send, restart, end), the model listing (instant
from the cache, one background refresh), and opening before/after versions
with the user's own applications; `sink.rs` is the bridge that streams the
conversation's narration into the window; `signin.rs` runs the opening screen's
"Sign in with ChatGPT" button — one sign-in at a time, cancellable, its
progress and outcome events (`sign-in-step`, `sign-in-done`) rendered beneath
the button, and the account it wins arriving as the same `models-refreshed`
the picker already renders through; with no account set up the sign-in is the
screen's primary button and the folder picker waits for it; `review.rs`
translates the workspace vocabulary into cards and runs the
gentle-then-explicit conflict flow; `synod/src/main.rs` runs exarch's
`dispatch_pre_main` re-exec trampoline first, like every
[[invariants/single-binary|multicall]] binary here.

The frontend is one file, `synod/ui/index.html` — markup, style and script
together — beside the three libraries it vendors and the nothing it fetches:
`marked.min.js` (GFM), `purify.min.js`, and `katex/` (KaTeX 0.18.1, its
stylesheet and its twenty `woff2` faces; the only web fonts the app ships).
Assistant prose is markdown with TeX, and `renderAssistantMarkdown` is the
single path model text takes to the DOM. Its order is the load-bearing part:
each formula is **lifted out before marked sees it** — markdown would read
`x_1 * x_2` as emphasis, and `breaks: true` would cut a multi-line `$$` with
a `<br>` — leaving a private-use sentinel that the prose carries through
marked and DOMPurify unharmed; the typeset formula is put back into the
scrubbed tree afterwards, because DOMPurify's CSS filter would strip the
inline metrics KaTeX's layout is made of. That is safe only because KaTeX
runs with `trust` off, where it can emit neither a link nor a raw node: the
scrub still covers everything that came from the model as markup. An
unterminated formula is not one, which is what keeps a half-streamed `$$`
from flashing red while it arrives; a `$` inside code, beside a space, or
against a digit (`$5-$10`) is a dollar sign, not a delimiter. Code is judged
twice, because it must be: the scan before marked knows only fences and
backticks — whether four spaces open a code block or continue a list item is
a question only a block parser can answer — so a sentinel that marked put
inside a `<code>` is handed back as the text it was written as. No message
wears a name above it — who spoke is said by the bubble's side and colour.

## vm-manager/ — the machine

One trait each side of a boot: `Hypervisor::boot(&MachineSpec) ->
Result<Box<dyn Machine>, Error>`, with `MachineSpec::resolve` the one
platform-independent judgment of a spec, called by every backend so a bad
spec is refused in the same words everywhere. `BootArtifact::resolve` is its
twin for the media, and makes every file absolute: the paths are opened by
*another process* — `vmcompute` runs in `C:\Windows\System32` — so a relative
path that resolved for the caller names nothing by the time the machine is
built. `Machine::take_wires` is the one signature that varies — `Wires` holds
an `OwnedFd` per wire on Unix and an `OwnedSocket` per wire on Windows —
because each platform owns its own accepted sockets, and both are adopted by
the host seam unchanged.

**The crate boots only real machines.** `detect(Option<BootMedia>)` answers
`Vz`, `Brokered`, or `Hyperv`, or refuses with a sentence for a
non-programmer: not a platform with a hypervisor at all, no boot media, a
macOS build unsigned for virtualization, or a Windows account the compute
service will not serve. On Windows the order is the machine broker first —
which needs no boot media from this process at all, since the service has its
own installed beside it — and only then the in-process `Hyperv`, which is a
checkout rather than an installation. There is deliberately no software
fallback: a synod that cannot put hardware between the agent and the rest of
the computer refuses to start rather than degrade to a weaker mode.
`examples/boot-smoke.rs` is the human-driven boot, one body over both
backends.

- `vz.rs` — `Vz`: Virtualization.framework, macOS arm64, bound through
  `objc2-virtualization`. It builds and validates the full configuration —
  direct kernel boot, RO rootfs + RW sparse session disks, a virtiofs share
  of the granted folder with `read_only` as the mount's law, a vsock device
  and no network device, console to the host log — drives the `!Send`
  machine from a dedicated thread against a private serial dispatch queue,
  and declares boot only when the guest's daemon has dialled both the
  control port and the net port. One socket device multiplexes them: a
  second network *device* is exactly the fix that must never be made, and a
  test asserts `socketDevices().count() == 1` to say so.
  Those accepted connections are the host ends of the §3 control plane and
  the §6 net wire: `Machine::take_wires` hands both out exactly once (a
  second ask panics as a caller's bug), and a second guest dial is refused. Booting requires the
  `com.apple.security.virtualization` entitlement — `vz::entitled()` is a
  process check, not a platform check.

## vm-manager/src/hcs/ — the Windows machine

`Hyperv`: Hyper-V through the Host Compute System API — `computecore.dll`, the
surface the Virtual Machine Platform feature provides and the one WSL 2 and
Linux containers are built on. This is the module the broker below runs in its
own process; `detect` reaches it directly only in a checkout. `available()`
answers in one of three remedies rather than one failure: the feature is not
installed, this account is outside the computer's local **Hyper-V
Administrators** group, or the compute service is not answering at all. An HCS
system has no thread affinity — it is a handle, not a queue-bound object — so a
`Guest` holds its machine directly and the only threads in the backend serve
blocking I/O.

- `mod.rs` — `Hyperv`, `Guest`, `available()`, the refusal texts, and the table
  of correspondences with `vz.rs`. `boot`'s order is load-bearing at three
  points: the console pipe exists before the machine that names it, the
  control-plane listener is bound before the machine *starts*, and
  `HcsGrantVmAccess` runs on the four boot files before a worker process opens
  them as its own virtual account. `Guest::stop` closes the wire first, so the
  guest powers itself off from inside, then revokes every access entry it
  granted, so none naming a dead per-machine identity is left on anyone's
  folder; `Drop` shares that path. Three constants carry the teardown's own
  patience — `REMOVE_GRACE`/`REMOVE_PULSE`, over which `remove` waits out the
  worker process that holds the session disk past `Stopped`, and `ORPHAN_AGE`,
  above which `Hyperv::new`'s `sweep_orphans` reclaims what earlier runs left;
  `session_disk_epoch` is what makes that sweep incapable of naming anything but
  a session disk
  ([[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]]).
  Both dial timeouts end in `console_says`, which quotes the guest's own last
  lines and names its log, and `Guest::dialled` is what decides whether that log
  survives the teardown
  ([[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]).
- `api.rs` — the entry points, resolved with `LoadLibraryW`/`GetProcAddress`
  rather than statically imported, so a Windows without the feature gets a
  sentence instead of a process that will not start. `Api::settle` holds the
  whole operation protocol — mint an operation, hand it to the call, read the
  real outcome and the service's own JSON error text out of
  `HcsWaitForOperationResult` — in one place, and `HCS_E_ACCESS_DENIED` is the
  one code recognised rather than merely reported.
  `HcsGrantVmAccess`/`HcsRevokeVmAccess` are looked for in `computecore.dll`
  *and* `computestorage.dll`, because they are exported by the former —
  Microsoft's own documentation and Go binding name the latter, which on
  10.0.26100 exports neither.
- `spec.rs` — the machine as one JSON document, since HCS takes no builder
  objects: `Chipset.LinuxKernelDirect`, `ComputeTopology`, `Devices.Scsi` (the
  rootfs at LUN 0, the session disk at LUN 1), `Devices.Plan9` (the granted
  folder, with `LINUX_METADATA` always and `READ_ONLY` when the grant is),
  `Devices.HvSocket`, `Devices.ComPorts`,
  `ShouldTerminateOnLastHandleClosed`, and **no network adapter at all** —
  absent, not disabled. Being data rather than a sequence of setter calls, the
  document is built and read back under ordinary `cargo test` with no machine
  and no privilege. `kernel_command_line` writes `ral.port` and `ral.plan9`
  from named fields, never positionally: the guest would mount its own control
  plane if the two were ever swapped.
- `hvsock.rs` — the control plane. `service_guid` is the entire bridge between
  the two addressing schemes: a Linux vsock port `p` is the service GUID
  `pppppppp-facb-11e6-bd58-64006a7986d3`, which is why a guest that knows
  nothing of Windows can still be dialled. `socket_sddl` names SYSTEM,
  built-in Administrators, and *this user's own SID* — never a wildcard, since
  this socket is one of two doors into a machine with no network *adapter* of
  its own (the guest's actual network rides the second `HvSocket` port,
  `NET_PORT`, into a host process — [[design/egress|egress]]) — and
  `fresh_machine_id` draws on `ProcessPrng` because the machine's identifier
  *is* half the socket's address.
- `vhd.rs` — a `VirtualDisk` attachment must be a VHD, so the raw ext4 images
  are wrapped as **fixed VHDs**: the sectors verbatim followed by one 512-byte
  footer, which makes wrapping an append rather than a conversion — no block
  map, nothing transcoded, the filesystem identically placed.
  `ensure_rootfs_vhd` does it once into `%LOCALAPPDATA%\Synod\Machine\` behind
  a marker recording *which* image was wrapped, and passes a shipped `.vhd`
  through untouched; `create_session_vhd` makes the session disk, which the
  guest formats on every boot and the machine's teardown deletes. That one is a
  **dynamic** VHD declaring 8 GiB — ~18 KB of metadata when empty — because
  Hyper-V refuses a virtual disk whose *file* is sparse (`0xC03A001A`), so
  growth has to be the format's business rather than the filesystem's.
- `console.rs` — the guest's `ttyS0` on a named pipe the compute service dials
  as a client, because without it a boot that failed and a boot that is merely
  slow are the same timeout. The pump *tees*: `stdout`, a per-machine
  `synod-console-<id>.log` in the same cache the disks live in, and `Tail`, a
  ring of the last lines the boot failure quotes. `RETAINED_LINES`, `LINE_LIMIT`,
  `LOG_LIMIT` and `LOG_LIFETIME` are the four bounds that keep the diagnostic
  from becoming litter, and `discard` is what a boot that dialled calls on its
  own log ([[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]).
  `Console::wake` connects to its own pipe to release a pump parked on a machine
  that never started.

## vm-manager/src/broker/ — the privileged half, on Windows

The service that owns the machine so the window does not have to, and the
client that asks it. One instruction crosses (`Request::Boot` — a folder and a
read-only flag), and everything else about the machine is the service's own
([[decisions/260725_windows-machine-broker|windows-machine-broker]]).

- `mod.rs` — the protocol and the argument: `PIPE`
  (`\\.\pipe\synod-machine-broker`), `VERSION` checked before anything else,
  the `Request`/`Reply` pair, and length-prefixed JSON with a 64 KiB frame cap
  written out here rather than borrowed from `ral-core` (a machine layer that
  needed the shell to talk to its own service would have the dependency
  backwards). `Request::Adopted` is the third step of `Boot` → `Booted` →
  `Adopted`, and its doc is where the ordering trap is stated.
- `client.rs` — `Brokered`, a `Hypervisor` that asks rather than acts, so
  `detect` can prefer it without `synod::session` or the seat knowing which
  backend it got; `Brokered::available()` is the probe `detect` asks.
  `adopt_socket` turns the service's `WSAPROTOCOL_INFOW` bytes back into an
  `OwnedSocket`. `BrokeredGuest` holds the pipe as the lease: `shutdown` and
  `Drop` both close it, and closing it is what stops the machine.
- `service.rs` — the only privileged code synod ships, and the file to review:
  `PIPE_SDDL` (`D:P`, SYSTEM + built-in Administrators + *interactively
  logged-on* users, never `AU` and never a wildcard), `serve`/`serve_client`
  (one machine per connection, held in the serving thread's local),
  `readable_by_client` (`ImpersonateNamedPipeClient`, the folder opened as the
  caller, reverted by a `Drop` guard on every path out including an unwind),
  `client_process` (`GetNamedPipeClientProcessId` — the kernel's answer, not
  the client's), `describe_socket` (`WSADuplicateSocketW` for that one process
  id), `media` (the boot artifact beside *this* executable), and `cache`
  (`%ProgramData%\Synod\Machine`, machine-wide because the wrapped rootfs is
  identical for every user and `LocalSystem`'s `%LOCALAPPDATA%` is SYSTEM's
  profile).
- `vm-manager/src/bin/synod-machine-broker/` — the program: `main.rs`, the
  entry point every platform gets, since a Cargo binary target belongs to the
  package and not to a platform; and `service.rs`, the two ways it starts on
  Windows — the service control dispatcher (`SERVICE_NAME` =
  `SynodMachineBroker`, report `RUNNING` before serving, stop by process exit
  since the threads own the machines), and `--console`, the same behaviour with
  a terminal attached, which is how a maintainer sees the guest's own console
  say why a kernel did not come up.
- `synod/wix/broker-service.wxs` — the installer side: a WiX fragment
  (referenced from `synod/tauri.windows.conf.json`) declaring the service into
  `INSTALLDIR`, so it shares the one `boot\` directory with the application;
  `LocalSystem`, automatic, started at install, removed on uninstall.
  `just broker-install` / `broker-uninstall` do the same from a checkout with
  `sc.exe`.

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

`Boot::workspace` is where the guest learns which hypervisor's folder it has:
an `Export`, either `Virtiofs { tag }` or `Plan9 { name, port }`, decided by
whether `ral.plan9` names a port on the command line. The 9p arm is the one
mount whose options cannot be written before the mount is attempted, because
`trans=fd` names the descriptor of a *fresh vsock connection to the host's
server* — the daemon dials, sizes the socket's buffers, and mounts
`trans=fd,rfdno=N,wfdno=N,msize=…,version=9p2000.L,aname=<share>` over it.
`ral.plan9` and `ral.port` are two sockets for two jobs and are read by name,
never by order.

The `ral.` key set and its value grammar are one versioned agreement, and
`boot.rs` is where the version lives, beside the command line's only writer and
only reader: `boot::CONTRACT`, `MANIFEST_KEY`, and `check_media`, the judgment a
*host* build runs over the media it is about to package
([[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]]).
`ral-daemon/examples/boot-contract.rs` prints that constant and nothing else, so
the media's manifest records a number it compiled rather than one it read.

## vm-image/ and ral-initramfs/ — the boot media

The design record's §7 built, and `ARCH`-parametric over synod's two guests:
`ARCH=arm64` for Virtualization.framework, `ARCH=amd64` for Hyper-V. A build
refuses a container that is not its own architecture rather than let qemu-user
emulation quietly produce something else. `build.sh` assembles the rootfs — a
pinned **Ubuntu 26.04 LTS (resolute)** office userland (LibreOffice headless,
the Python document stack, pandoc, OCR, wide fonts, full locales, no
toolchain) via mmdebstrap → ext4 → zstd in a native container, checksummed and
version-manifested. `build-boot.sh` builds the boot pair, stamping the git hash
and `boot_contract=` into `boot-manifest.txt` — the one line a host build reads
back — and the kernel is where the two guests part: arm64
takes Ubuntu's generic kernel apart to the raw Image `VZLinuxBootLoader` wants,
amd64 keeps that same `vmlinuz` verbatim, because it already *is* the bzImage
`LinuxKernelDirect` loads. So do the module sets — virtio on arm64; on amd64
`hv_vmbus` and the drivers on it (`hv_storvsc`, `hv_utils`, `hv_balloon`),
`vsock` + `hv_sock`, and 9p as three modules: `9p`, `9pnet`, and **`9pnet_fd`**,
the trap worth naming, since upstream split `trans_fd` out and a `trans=fd`
mount with `9pnet` loaded and `9pnet_fd` missing fails with a bare `ENODEV`.
There is deliberately no `hv_netvsc`: the guest has no network device to drive.
`vm-image/README.md` records the corrections of §7's prose to real package
names and the open questions (squashfs, distribution, determinism).

`ral-initramfs/` is the initramfs itself, and every decision in it is a typed
plan — assemble the overlay root, make the session disk, install daemon and
engine, `switch_root` — unit-tested on any machine. It hardcodes no disk:
`plan.rs`'s `resolve_disks` probes candidate *pairs* in order, virtio
(`/dev/vda` + `/dev/vdb`) first, then Hyper-V's SCSI (`/dev/sda` + `/dev/sdb`),
and refuses by naming every candidate it looked for.

## What is not here

Two things about the Windows machine, neither of them settled by the code
compiling:

- **A completed guest boot is not witnessed yet.** What is: a machine created
  and started through the broker, booting a kernel and an initramfs that formats
  the session disk and reaches the daemon — known because the daemon refused a
  command line carrying a `ral.` key its own build predated, and said so on its
  own console. What is not: a guest that finishes booting and dials, since the
  media rebuilt against this host's boot contract has not been booted yet. The
  path is otherwise compiled, clippy- and rustdoc-clean, and unit-tested wherever
  a test can reach without a machine — the document's shape, the VHD footer's
  checksum and geometry, the port→service-GUID mapping, the socket's own
  descriptor, the pipe's, the console ring's line bookkeeping, the release of a
  disk another process holds. `vm-manager/examples/boot-smoke.rs` and
  `synod/examples/boot-run.rs` are the vehicles for the rest.
- **Whether the host's 9p server can read the granted folder unaided is
  untested.** `HcsGrantVmAccess` is called on the four boot files, which a
  worker process really does open as its own virtual account, and deliberately
  *not* on the user's folder. The broker's impersonation check answers a
  different question — may the *caller* read it — so if a guest's mount is
  refused, a session-scoped grant is still the knob.

Also the image pipeline's open questions above. The rest of the design record
runs — end to end on macOS, and everything above the machine on both: the §3
wire carries real runs (`synod/examples/boot-run.rs` witnesses
boot → shared folder → engine → settled report), the §5 spawn jail stands
inside the guest (a fresh uid and a cgroup between the engine and what it
runs), and §6 gives the guest a network of its own — a `tun` whose only peer
is `guest-net`, a user-mode TCP/IP stack in a host process — rather than the
single `fetch-url` verb an earlier draft made the whole egress surface
([[design/egress|egress]],
[[decisions/260727_the-guest-gets-a-network-not-a-verb|the-guest-gets-a-network-not-a-verb]]).
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
- [[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]] — why the
  Windows machine is assembled the way it is.
- [[decisions/260725_windows-machine-broker|windows-machine-broker]] — why a
  `LocalSystem` service creates it, what the Hyper-V Administrators group would
  have cost every user, and what keeps the service's surface narrow.
- [[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]] —
  why the kernel command line carries a version, and why the build rather than
  the boot is where a host/guest mismatch is caught.
- [[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]
  — why the guest's console is teed to a log and quoted in the failure, and why
  the broker protocol did not have to change to carry it.
- [[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]]
  — why teardown waits out the worker process, and why a starting backend sweeps
  the cache.
- [[map/core/transport|core / transport]] — the framed seam the control plane
  rides, whose stream type is std's owner for a connected socket and not a claim
  about the address family.
