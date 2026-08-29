---
status: active
---

# The Windows privilege lives in a service, not in the user

**Synod's window runs with exactly the rights of the person who opened it and
asks a `LocalSystem` service — installed by the MSI, started by Windows — for
the machine it needs; the privilege the platform demands for a virtual machine
moves to the code that needs it rather than to everyone who runs the
application.** The requirement is plain and it is the whole of the argument: an
ordinary `.msi` installation, after which the person who opens synod needs no
permission they did not already have. That requirement is not satisfiable by
choosing the machine's parts more carefully — it is a question about which
*process* holds the privilege, and so it is answered by a second program rather
than by another line of a JSON document
([[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]] settled
the document).

## Context

The Host Compute System API answers `HCS_E_ACCESS_DENIED` to any caller who is
neither an administrator nor a member of the local *Hyper-V Administrators*
group. The backend decision recorded that as a deployment step — a group an IT
department fills with one line — and that is a defensible reading when the
fleet is a fleet. It is the wrong reading when the unit of installation is one
person double-clicking an installer, which is what synod is now held to. Synod's
user is a university secretary who is not an administrator of her own computer
and must not become one, nor acquire anything that resembles becoming one, in
order to grant a folder and ask for a letter to be filed.

Windows has three answers for a program in this position, and they are not three
spellings of one thing: put the user in the group, ask a privileged service
somebody else already ships, or ship a privileged service of one's own. The
first two lose, and it is worth being precise about *why* each loses, because
both look cheaper than the third.

## Decision

**Not the group, and the reason is a local privilege escalation rather than
tidiness.** *Hyper-V Administrators* is not "may run synod"; it is "may build an
arbitrary virtual machine", and one of the things an arbitrary machine may be
given is a **physical disk**. A guest reading a raw disk reads it at the block
layer, beneath NTFS and therefore beneath every access-control entry the
filesystem would otherwise enforce. Filling that group with synod's users would
hand each of them the ability to read files they are denied, in exchange for a
capability none of them asked for — and it would do so in order to run an
application whose entire claim is that the agent can see one folder and nothing
else ([[design/grant|grant]], [[design/two-enforcers|two-enforcers]]). The
remedy would defeat the thing it was meant to enable, which is the sharpest form
a wrong answer can take.

**Not brokered through WSL either, and the privilege argument does not revive
it.** Asking `wslservice` for a Linux environment needs no privilege at all,
which makes it the cheapest thing a Windows program can do and the most
tempting once privilege is the problem. It remains refused for the reason the
backend page gives: a WSL distribution is a guest in a *shared* utility VM, with
`/mnt/c` mounted, with Windows interop wired into the guest's `binfmt_misc`, and
with siblings that outlive any one session. That is the negation of "one folder
and nothing else", and no amount of privilege saved buys it back. What synod
needs from Windows is not a Linux environment; it is an empty machine, and an
empty machine is something one has to be entitled to make.

**So a broker of synod's own: one `LocalSystem` service that owns the machine's
lifecycle, and an unprivileged client that asks.** This is not an invention but
the settled shape for this position on the platform — WSL has `wslservice`,
Docker has `com.docker.service`, VirtualBox has `VBoxSDS` — and in every case an
unprivileged client asks a privileged broker to make a machine and receives back
what it needs to use it. `LocalSystem` and an automatic start are not
preferences: the compute service serves `SYSTEM`, so a broker that is less than
`SYSTEM` cannot broker; and a machine may be wanted by a session that has not
begun, so the service comes up with the computer and survives every logon and
logoff for the life of the installation.

**What makes a broker safe is not its size but the narrowness of what it
accepts.** This one takes exactly one instruction — *boot a machine over this
folder* — and constructs the machine itself. None of the parameters that would
make a broker dangerous is client-controlled:

- the **kernel, initramfs and rootfs** are the ones installed beside the
  service's own executable, so the media is a property of the installation
  rather than of the conversation, and no request can name a kernel;
- the **devices** are the backend's fixed list — two disks, one share, one
  socket, one console, and no network adapter. There is no pass-through of
  anything, so there is no request that could ask for one;
- the **resources** are absent from the protocol altogether: the spec is built
  from the folder by the service, so a caller cannot ask for a machine larger
  than the computer;
- the **frame** is bounded before a byte is allocated for it, because the one
  thing a hostile client on a privileged service's pipe could otherwise ask for
  is that service's memory;
- the **folder** is the one thing the client does supply, and it is checked
  rather than trusted.

That list is the argument, and it is falsifiable: if a future request field ever
reaches the backend without passing a check in
`vm-manager/src/broker/service.rs`, this page stops being true. That is the
thing to review, and the module says so in the same words.

**The folder is checked by impersonating the pipe's client, not by asking
whether the service can read it.** `ImpersonateNamedPipeClient` puts the
caller's token on the serving thread, the folder is opened under it, and the
token is dropped again by a guard that runs on every path out including an
unwind — a thread left wearing one client's token would answer the *next*
client's question as that one. The alternative is not merely weaker, it is
inverted: a service running as `LocalSystem` can read everything, so checking as
itself would let one user have another user's documents mounted into their own
guest. That is a worse hole than the one the service exists to close, and it is
the hole a broker gets wrong by default.

**The control socket crosses to the client by description, targeted at the
kernel's answer to who is asking.** The broker accepts the guest's control-plane
connection, but the engine's frames must reach synod, and a socket is not a
thing one can put in a JSON reply. Windows' mechanism is `WSADuplicateSocketW`:
the owner asks for a description of the socket valid for one named process, and
that process turns the description back into a socket of its own, after which
both ends are peers on the same connection. The process it is duplicated *for*
comes from `GetNamedPipeClientProcessId` — never from anything the client said
about itself. A client trusted on its own process id could otherwise have a live
socket into a guest planted in someone else's process, which is the same class
of mistake as trusting a path without opening it as its claimant.

**The handshake has three steps because both the earlier and the later moment
are wrong.** `Boot` → `Booted` → `Adopted`: the broker holds its own handle on
the control socket until the client says it has made one. Closing it sooner
races the duplication and can leave the client holding a socket whose connection
is already gone. Never closing it is worse and quieter — two handles keep the
connection open, so the guest never sees the end-of-file that a closing client
is supposed to cause, and the inside-out power-off every teardown depends on
simply never starts. That failure is a machine which stops only when forced, for
a reason no log would show, so the ordering is paid for with an extra message
rather than left to chance.

**The connection is the lease.** A machine lives exactly as long as the pipe
connection that asked for it, held in the local of the thread serving that
client; when the client asks it to stop, exits, or crashes, the thread drops it
and the machine is torn down with its session disk. Nothing has to be reaped
later and nothing outlives the process that owns it, which is the same law
the [[design/engine-protocol|engine protocol]] gives the engine's own wire, applied one
layer down. It is also why a service stop is
honoured by process exit: unwinding those threads politely would mean tracking
them, a second bookkeeping of live machines beside the one the connections
already are.

**Detection prefers the broker and asks the application for no boot media at
all.** `vm_manager::detect` on Windows tries the service first, because that is
the path an installed synod is on and the service has its own media installed
beside it — so the question of whether *this process* was shipped an image does
not even arise for a user. Only when no service answers does it fall back to
creating the machine in-process, which is a checkout rather than an
installation, and whose refusal now names the installer's own answer so a
maintainer is not left thinking the group is the only route.

## Alternatives considered

- **Elevating synod, or asking for elevation at the grant.** The application
  hosts a language model driving a shell; it is the last process on the computer
  that should hold administrative rights, and the whole product is an argument
  about what the agent cannot reach. On a managed machine the user cannot
  elevate anyway, so this fails the requirement outright as well as on
  principle.
- **Forwarding the whole `MachineSpec` to the service.** Shorter, and
  symmetrical with the in-process backend, which is exactly what makes it
  attractive and what makes it wrong: every field of the machine document would
  become client-controlled, including the media and the device list, and the
  narrow-surface argument above would have nothing left to rest on. Only the
  folder and the read-only flag cross; the client resolves the folder as well,
  so a missing one is refused in the same words on both backends rather than
  arriving as a service's second-hand complaint.
- **A registry of live machines, reaped on a timer.** The obvious shape for a
  service that owns objects on behalf of clients, and unnecessary here: the
  connection already *is* the record, held by the kernel, correct by
  construction across crashes. A table would be a second copy of that fact,
  with the usual obligation to keep it honest and the usual failure when it is
  not.
- **`AU` (authenticated users) on the pipe.** The obvious descriptor and one
  step too wide. The pipe admits `SYSTEM`, the built-in Administrators, and
  *interactively logged-on users* under a protected DACL, so nothing inherited
  widens it; `IU` is satisfied only by a session someone is actually sitting at,
  which excludes a network logon and excludes other services. Synod is a desktop
  application, and nothing else on the computer has business asking for a
  virtual machine. A wildcard is refused here for the same reason it is refused
  on the guest's control socket.
- **Borrowing `ral-core`'s framing for the protocol.** The length-prefixed JSON
  is written out in the machine crate rather than taken from the shell's
  `subprocess_codec`, at the cost of a few dozen duplicated lines. A machine
  layer that needed the shell in order to talk to its own service would have the
  dependency backwards, and `vm-manager` deliberately keeps that direction
  clean.

## Consequences

- **A privileged service is part of what synod ships, and it is the thing to
  review hardest.** `vm-manager/src/broker/service.rs` is the only place where a
  message from an unprivileged sender reaches code running as `SYSTEM`; the
  review question is not "is this code correct" but "is the machine's document
  still uninfluenced by the request beyond the folder and the read-only flag".
  Everything else the application does to the user's folder it still does as the
  user, with the user's own rights, exactly as on macOS.
- **A Windows installation now holds two executables sharing one boot
  directory.** `synod-machine-broker.exe` sits beside the application in
  `INSTALLDIR` precisely so that one copy of a two-gigabyte rootfs serves both.
  It stands outside the language runtime the way `ral-sh` does
  ([[invariants/single-binary|single-binary]]): no `ral-core` dependency, no ral
  semantics, no shell — it starts machines and hands back a socket.
- **The installer declares the service rather than scripting it.**
  `synod/wix/broker-service.wxs` is a WiX fragment referenced from
  `synod/tauri.windows.conf.json`: `SynodMachineBroker`, displayed as *Synod
  machine broker*, `LocalSystem`, automatic, started at install so the first
  grant does not wait for a reboot, stopped on upgrade as well as removal, and
  removed on uninstall — so an uninstalled synod leaves no `SYSTEM` service
  pointing at a deleted binary. `just broker-install` / `broker-uninstall`
  register the same service from a checkout with `sc.exe`.
- **Two halves can now differ in age, so they check.** The MSI installs both,
  but a developer runs one from `target\release` against the other from
  `Program Files`; a protocol version mismatch is refused loudly, in a sentence
  naming both versions, rather than discovered later as a strange field — the
  same discipline the engine's own `Attach` carries.
- **The service's cache is machine-wide.** Its disks live under
  `%ProgramData%\Synod\Machine`, because `LocalSystem`'s own `%LOCALAPPDATA%`
  would be `SYSTEM`'s profile — writable and semantically wrong — and because
  the wrapped rootfs is identical for every user of the computer, so it is
  written once rather than once per profile. The in-process backend keeps
  `%LOCALAPPDATA%\Synod\Machine`, which is one profile's business.
- **The refusals gain a remedy and lose one.** "This account may not use
  Hyper-V" stops being a user-facing sentence on an installed synod and becomes
  the developer's own affair; in its place are "the service is not answering"
  (naming it as Windows displays it, so IT can start it) and "synod and its
  service are different versions". A synod that cannot put hardware between the
  agent and the computer still refuses to start rather than degrade.
- **The service can be run as a console program, and that is how a boot is
  watched.** A service's `stdout` goes nowhere, so the guest's own console — the
  thing that says why a kernel did not come up — is invisible in the mode users
  run. `--console` is identical behaviour with a terminal attached, which makes
  it the maintainer's mode rather than a lesser one.

## Open questions

- **The guest's own boot is still being verified.** A machine is now created and
  started through the broker on an account with no special membership, which is
  what this decision was for; whether the kernel comes up and the daemon dials
  the control plane is what the console pipe and
  `vm-manager/examples/boot-smoke.rs` are watched for.
- **The folder question the backend decision leaves open is unchanged by the
  broker.** Impersonation answers *may the caller read this folder*, which is a
  different question from *may the machine's own worker process open it*. If a
  guest's 9p mount is refused, an access grant scoped to the session is still
  the knob, and it should not be left standing on someone's documents.
- **Nothing bounds how many machines one person may ask for.** Each connection
  owns at most one, and a second `Boot` on the same connection is refused, but
  the pipe admits unlimited instances and each machine declares real memory. The
  count is a policy question a reviewer should settle deliberately rather than a
  gap to be discovered by a user with many windows open.

## See also

[[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]] (the
machine this service creates, and the deployment row this page replaces),
[[map/synod|synod]] (where both halves live),
[[design/engine-protocol|engine-protocol]] (the lease law
one layer up: a connection is a lifetime),
[[decisions/260721_synod-is-a-second-product|synod-is-a-second-product]] (whose
audience makes an unprivileged installation a requirement rather than a
courtesy),
[[design/two-enforcers|two-enforcers]] (the machine is the outer ceiling, and
this is how it is obtained without widening authority elsewhere),
[[decisions/260713_projection-keyed-appcontainer|projection-keyed-appcontainer]]
(the same "authority is exactly what was declared, checked by the kernel"
discipline inside the guest's host),
[[invariants/single-binary|single-binary]] (the runtime rule this service stands
outside of),
`dev/docs/VM/SYNOD.md` §2 (the platform table), `synod/wix/broker-service.wxs`
(the installer's own account of the same argument).
