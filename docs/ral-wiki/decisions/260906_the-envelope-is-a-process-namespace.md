---
status: active
generated_at_commit: 39934600
---

# The envelope is a process namespace

**A grant's envelope confines process identity and lifetime, not only the
filesystem and network — on every projection, wherever the host can build the
namespace, and it says when it cannot.** What `grant` means for `ps`, `kill`,
orphans and `detach` changes accordingly.

## Why

- Under `grant [fs: [read: [$dir]]]` a child could read every host process's
  `cmdline` and `environ`, `kill` every same-uid process, and `shmat` any
  same-uid segment. A grant that withholds `~/.aws` and hands over `ps -ef` is
  not the mechanism it claims to be.
- Process identity is authority, and an envelope that leaves it ambient is a
  view that shows the whole room through one wall
  ([[design/two-enforcers|two-enforcers]]).

## The process model, corrected

bwrap never execs the payload in place: its monitor clones a namespace init,
and the init forks the payload. `--new-session`'s `setsid` runs in the payload,
so the payload's pid is the session and the group of everything inside. Every
ladder used to signal the *monitor*, which died and took the payload down by
`PDEATHSIG` with zero grace — Ctrl-C hard-killed a confined command, a `trap`
never ran.

## Decision

- **Namespaces on every launch**, keyed on nothing in the grant:
  `--unshare-ipc --unshare-uts --unshare-cgroup-try` unconditionally, and
  `--unshare-pid` iff the host can mount a fresh procfs
  (`HostEnvelope::private_pids`, probed once per process). `--proc /proc` on
  both projections; its meaning follows bwrap's own rule — a fresh table with
  the namespace, a bind of the host's without — so `/proc/self` is always the
  payload's own.
- **The ladder addresses the payload's session, never the supervisor.** A
  `Kept` launch passes `--info-fd` and reads back `child-pid`; that pid is the
  group. `kill(-P, grace)` reaches the payload — a namespace init drops every
  signal from outside but `SIGKILL` and `SIGSTOP` — and `kill(-P, SIGKILL)`
  ends the init and with it the namespace. The monitor leads an inert group
  nobody signals. An enveloped launch is `NewLeader` whatever was asked, the
  foreground is never handed to it, and a confined pipeline stage's group is
  addressed beside the pipeline's
  ([[decisions/260905_one-delivery-path|one-delivery-path]]).
- **bwrap's `128 + n` is the payload's death by `n`**, attributed to the
  teardown that sent `n` exactly as a `Signaled(n)` is.
- **`Ownership` decides the two ties** — death (`--die-with-parent`) and
  address (`--info-fd`) — and nothing about what the envelope *is*. A
  `Surrendered` survivor keeps every namespace: its init outlives the session,
  so a daemon runs confined until it exits; the receipt's `pid` names the
  monitor, dead the moment a self-daemonizing program hands it back; the
  survivor's `getpid()` is namespace-local; from inside any later grant it is
  invisible — the frame that cannot name it to widen it cannot name it to kill
  it. A `Kept` payload's orphans die with the envelope: `spawn`, `service` and
  `detach` are the verbs for outliving a command.
- **A host that cannot build the namespace is reported, not refused.** A grant
  promises what it names — `fs`, `net`, `exec` — and nothing in it names
  process reach; the namespace is an invariant of being under an envelope, like
  `--die-with-parent` and the seccomp blocklist, applied where the host allows.
  Container runtimes that mask `/proc` (crun, runc, Docker) leave locked mounts
  the kernel will not let a fresh procfs cover in a user namespace; there the
  payload sees the container's own table, and `RAL_DUMP_SANDBOX_PROFILE` prints
  the `HostEnvelope` naming the unheld invariant, its cause, and `--privileged`
  as the flag measured to lift it (`unmask=ALL` does not, on rootless podman).
  Where `--dev` is refused, `/dev` is built by hand over the host's `/dev/pts`
  and reported the same way.
- **The datum is the host's, not the launch's.** One probed value per process,
  read where host facts are read; no mark per launch, since a constant carries
  no information.
- **Nothing on Linux enters a sandbox from inside.** `--sandbox-projection` is
  refused there as on Windows; a nested envelope is the one thing that must
  not happen.

## Rejected

- *Fail closed where the namespace cannot be built*: kills every confined
  launch in every rootless container for a property no grant asked for.
- *`--tmpfs /proc` as the fallback*: a different world, not a tighter one —
  `/proc/self` gone, every runtime that reads `/proc/self/exe` broken silently.
- *A process-reach axis on the projection*: it would exist only to have
  something to refuse, and its in-process half is already total. The day a
  *shared* process view is wanted, that is a widening to add.
- *Dropping `--new-session`*: leaves the monitor in the signalled group and
  reopens `TIOCSTI` (CVE-2017-5226).
- *`SIG_IGN` before exec, so the monitor survives the group signal*: inherited
  by the payload — the `nohup` disease under every grant.
- *`--as-pid-1`*: the monitor remains, and a payload with the default
  disposition silently drops the grace signal.
- *One namespace per session (`--pidns`)*: needs a session-lifetime init and
  an answer to which frames share a namespace, which the lattice does not
  give. Per-envelope is the meet-safe default; sharing is a later widening.
- *`--unshare-all --share-net`*: its manual defines it as "currently equivalent
  with …"; the argv should say what it means.

## Left open, deliberately

- A confined interactive command has no controlling terminal: Ctrl-C reaches
  ral and the ladder delivers a real `SIGINT`, but `open("/dev/tty")` fails and
  `SIGWINCH` never arrives. The fix — a trampoline inside the envelope that
  `setpgid`s, `SO_PASSCRED` for its pid, `tcsetpgrp`, `TIOCSTI` closed in
  seccomp — is a second design.
- Rootless containers still refuse the read-only rebind of their locked
  `/etc/hosts` and `/etc/resolv.conf`, so `Restricted` does not launch there.

Pinned by tests that spawn the envelope (`sandbox::linux::tests`): the grace
signal reaching the payload rather than the monitor, no host pid nameable
inside, `/proc/self` the payload's own on both projections, and a confined
pipeline stage's trap running (`ral/tests/pipeline.rs`). See
[[design/grant|grant]], [[internals/capability-enforcement|capability-enforcement]],
`docs/SPEC.md` §12.6 and §12.11.
