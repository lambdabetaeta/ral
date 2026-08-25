---
status: accepted
---

# The host dials in

**The hatchery existed because the connection was opened from the wrong side.**
Reverse the dial and the published preamble, the accept pump, the rendezvous
table, both TTLs, the pending-hatch table and the two-phase spawn all go — not
because they were badly built, but because nothing is left for them to do. The
direction of a connection is a design choice, not a property of the boundary.

## The founding observation

A guest reaches the host over vsock, and there were three such wires:

```text
1729  control plane   the daemon dials once; the connection is the trunk engine's transport
1730  net wire        the daemon dials once; the packet pump runs on it
1731  agent port      every hatched child dials afresh, forever
```

The daemon's own law states what the first two keep: *a connection that exists is
already proof the host is there, so there is no accept loop, no readiness
handshake, and no listening port inside the guest for anything to reach.* The
daemon dials, execs an engine, and hands it the fd. That is already the whole of
"make an engine and give it a wire to the host".

1731 was that same mechanism made **many-shot**, and many-shot is what cost
everything. Because the host accepted an unbounded number of dials and could not
tell them apart, it needed a token minted per spawn and stashed host-side, a
16-byte preamble to carry it decoded on both sides of the crate graph, an accept
pump per conversation peeking each dial with `MSG_PEEK` to route it, a rendezvous
table with two slot kinds because a dial might land before or after its awaiter, a
pending-hatch table holding the child's caps and log across the gap under a 10s
TTL, and two enquiry classes because the spawn now had a middle the host had to be
told about.

Every one of those is bookkeeping for *"which dial is this?"* — a question the
side that opened the connection never has to ask.

## The decision

**The guest binds a listener for the duration of one spawn.** It names the port in
its enquiry; the host dials it while answering, writes eight token bytes, and does
not answer until the child exists. One exchange.

This keeps the first two clauses of the daemon's law and **retires the third**:
there is now a listening port inside the guest, once per spawn. That is stated
plainly rather than paraphrased away, and it is smaller than it sounds — one port,
one token, one thread, no table.

- **The child exists before the roster names it**, literally, because the ack is
  written after `spawn()` succeeds. So the roster is honest with no state column,
  and there is no window in which a registered agent has no transport. Whether the
  child *boots* is liveness's job, exactly as it is for the trunk.
- **The listener thread waits for a peer only with the wake pipe beside it.** Two
  kinds of poll site and no clock: the listener, then the accepted dial before
  every partial token read. So a peer may send one byte and stop without pinning
  shutdown, and the builtin can always get its thread back. The one wait that is
  *not* a poll site is the seed write, and it is bounded instead — see below. No
  new number enters the design either way: the host's own wait on the ack already
  carries the transport's liveness deadline, and the seed write is given that same
  stall.
- **`Mooring.nursery` becomes `Mooring.fork`**, a sum of `Park(Nursery)` and
  `Listen`. The nursery was always the **identity** arm's door: the reentrancy law
  bars an in-process handler from holding `&mut Shell`, so the builtin parks a fork
  and the handler adopts it by id. The wire trunk had a nursery too, and it was a
  pen for no one — the builtin forked into it and adopted straight back out within
  the same dispatch. Now the mooring says which arm a run is in, because the
  nursery is present exactly when the identity arm is.

## The ack byte is not ceremony

The wire protocol offers no substitute: the host speaks first, and the only legal
first frame is `Attach`. So one byte, written once `spawn()` has succeeded and the
seed has crossed, is the **whole** of the guest's readiness signal. Without it the
roster answers for a child that may not exist, and "the child exists before the
host answers" degrades from a fact to a race. It lives beside the frame algebra —
the ack is the byte before the first frame — not in a module of its own; a module
for sixteen bytes is what died here.

## The seed channel is bounded, so the order is a law

The socketpair holds a couple of hundred kilobytes; a seed is as large as the
parent's scope. So the parent writes the framed `EngineSeed` only *after* spawning
and the child reads it *before* waiting for `Attach`, applying it once `Attach` has
selected and booted the installer. Neither side is ever waiting on protocol startup
while the other waits on the buffer, and no seed wedges the channel however far it
outgrows it. The buffer is not a limit on a seed's size; the framing codec's
256 MiB ceiling is the only size at which one fails, and it fails loudly.

Three consequences, each load-bearing:

- **The child's end is taken by value.** `spawn_engine` consumes it and closes it
  as it returns, so no code can write the seed while a reading duplicate still
  lives in the parent — that duplicate would leave the write blocked on a dead
  child instead of failing it with `EPIPE`. The ordering is not a comment; it is
  the signature.
- **The write is bounded, not cancellable.** By the time a seed is crossing, the
  child exists and is the parent's to reap, so what this wait needs is a bound
  rather than the wake pipe's cancel: it is given the same stall the engine allows
  its own protocol writes. A child that will not drain is a failed hatch, and the
  listener thread comes back either way.
- **A partly-sent seed kills its child.** Half a frame leaves the child blocked in
  its own read forever, and a child with no seed can never attach; it is killed and
  reaped on the spot rather than recorded for a sweep that waits on a death that
  will not come. That is the one hatch failure whose child is *not* left for the
  sweep, and the reason is exactly that the sweep could not notice it.

## The threat model, stated correctly

The earlier argument held that the spawn token was an **authority** binding — that
without it any guest process reaching the hatchery port could claim
`grant: dangerous`. **That argument is withdrawn.** Three things sink it: `narrow`
is a plain meet, so no child can exceed its parent whoever computes it; `Run.caps`
is enforced *inside* the guest engine, so capabilities were never a boundary
against a compromised guest — they are a policy between cooperating engines, and
the VM is the containment; and sibling agents in one VM share one kernel and one
real authority, so the grant between them is bookkeeping.

What the token actually secures is **adoption**: it stops an arbitrary guest
process from getting the host to build an `Agent` around its socket, which would
hand it a conversation with the model and a place in the model's context.
Impersonation and injection, not escalation.

The attacker is a **command the model spawned** — jailed by uid, cgroup and
`NO_NEW_PRIVS`, but with no seccomp filter, so `socket(AF_VSOCK)` is permitted to
it. It holds *less* than the engine, not more; the old claim that it "already holds
the VM's authority" was wrong about who it was. What it would gain by racing the
host to the port is a child engine seeded with the parent's scope — the parent's
bindings made readable to a process that should not read them.

A weaker thing is reachable without the token at all: a dial that connects and then
sends nothing keeps the listener in that one partial token read, so the host's own
dial is never accepted and the spawn fails on the host's deadline. Losing the token
race costs a stranger only its connection, but *stalling* costs the winner its
hatch. That is denial of one spawn and no more — no adoption, no scope read, and the
next enquiry binds a fresh port — so it is recorded here rather than defended
against; the defence would be a second poll set for pending dials, bought for a
threat the kernel's refusal already keeps off this image.

Measurement narrowed this further. The guest kernel **refuses**
`VMADDR_CID_LOCAL`, and a jailed command cannot read `/dev/vsock` to learn its own
CID at all (`EACCES`). So the kernel's refusal is the **standing defence**, and the
token is the second line, guarding a *guessed* CID rather than a read one.

The standing defence is in fact structural rather than incidental, and it is worth
naming precisely because it is checkable in one line. The boot image ships exactly
two vsock modules — `vsock` and `vmw_vsock_virtio_transport` (`boot-manifest.txt`'s
`modules_shipped`). Guest-local vsock routing is the *loopback* transport's job,
and no loopback transport is shipped. The virtio transport carries everything to
the host, so there is nothing in the guest that can route a connect to the guest's
own CID, whether that CID was read or guessed. (That the shipped set excludes it is
verified; whether this kernel could have it built in rather than as a module was
not.)

So on this image the token guards a threat that is **unreachable by construction**.
It stays anyway, and the reason is worth stating so nobody deletes it as dead
weight: the defence rests on a *build-time module list*, enforced nowhere near the
spawn, and one line added to `build-boot.sh` would make the threat live again. Eight
bytes and a comparison, on a path that already forks and execs, is a cheap standing
insurance against a change made somewhere else entirely. Nobody should restore the
*handshake* to defend a property it never had; nobody should remove the *token* on
the strength of a module list either.

## What was measured

- **Host→guest round trip: 840µs**, against real artifacts, bytes both ways.
- **An unbound guest port: `ECONNRESET` in 8–25ms across five boots**, from the
  guest's own vsock stack. Prompt, not a timeout — and a reset from the far side,
  which is proof the dial reached the guest at all.
- **Apple's documentation is wrong.** The generated binding for
  `connectToPort:completionHandler:` reads *"Does nothing if the guest does not
  listen on that port"*, which describes a no-op; the machine reports an error. A
  design built on the documented behaviour would carry a mandatory deadline it does
  not need. Recorded here because the next person to read that doc comment will
  believe it.

Getting to that measurement cost two fixes underneath it, and the second is worth
remembering: **no external command had ever run inside a synod guest.** The spawn
jail's cgroup setup had been broken since the day it landed, and every spawn died
with `guest jail: No such file or directory`. It went unseen because it is
`cfg(target_os = "linux")` and the one example that booted a guest used the `echo`
*builtin*, which spawns nothing.

## What died

`synod/src/hatchery.rs`'s pump entire, and with it an unswept `Slot::Dialed`
branch that leaked one host file descriptor per stray dial bearing valid magic,
uncounted by `refused_dials` because the magic was fine — fixed by deletion rather
than by a sweep. `exarch/src/fleet/hatch.rs` entire, leaving the desk's own two
cheap guards and `AgentRegistry::register`, which is authoritative for both the
name's form and its uniqueness. Both copies of the preamble. `AGENT_PORT`,
`Machine::accept_agent`, vz's `AgentReadiness` delegate and its listener
registration. `mint_token`'s host-side reservation, `agent_hatched`, `agent_abort`,
and engine-side `decode_hatch_answer`, `enquire_hatched`, `enquire_abort`.
`hatch`, `hatch_from_nursery` and `kill_hatched` — the `HATCHED` table's sweep is
the only reaper now, and nothing kills by pid.

**There are two vsock ports, not three.**

`AgentDial` survives against the plan's own deletion list: `connect_guest` returns
it, and it is the per-platform handle alias, so a literal `OwnedFd` does not
compile on the Windows backend.

## A bug this uncovered

`exarch::policy::narrow` had **never** been installed as core's grant narrower —
the registration hook had no caller anywhere in the tree — so `apply_seed` would
have refused every wire-seeded child at boot with *"this engine has no
grant-narrowing policy installed"*. The wire hatch had never run end to end. Fixed
here.

**The hook itself was then deleted.** A `OnceLock` a second crate must remember
to fill is a defect of shape, not of that commit: nothing makes the obligation
visible, so nothing catches it going unmet. `GrantNarrower` is now a third field
of `EngineInstaller`, chosen at `Attach` and therefore already in scope where
`apply_seed` runs — every host that boots an engine states the policy its seeded
children are held to, and the REPL, which hatches nothing, states a refusal in
one function. The same pass took `AF_VSOCK` out of core: `listen_for_hatch` is
handed a listening descriptor, exarch's `guest_port` binds it, and `ral-daemon`
carries its own `socket`/`connect` — which drops that init's dependency on the
whole language crate, the rule its own `reap.rs` already argues for. The sibling
hook, `sandbox::set_child_shell_extension`, is deliberately left: it runs in a
re-exec'd helper where no installer has been chosen, and an unfilled slot there
means *core builtins only*, a coherent shell rather than a refusal.

## Deliberately not done

`Seat` → `Helm` was requested during review and **dropped**. The argument stands
unrefuted — `core` already uses *seat* for a worker-admission seat, two concepts
sharing one word, and what `exarch::agent::Seat` *is* is the host's steering
position over a shell — but no parcel performs it and every wave is written in the
existing vocabulary.

## Consequences

Supersedes the spawn half of
[[decisions/260825_the-wire-carries-the-value|the-wire-carries-the-value]].
Amends [[decisions/260722_session-is-a-process|session-is-a-process]], whose
`agent-start` → `hatch` → `agent-hatched` sequence no longer exists.
[[decisions/260706_enquiry-channel|enquiry-channel]]'s closed *channel* set is
unaffected — a dial is a transport's provenance, not a channel — but its "the
engine asks, the host answers" table now sits beside a wire the host opens.

Windows is unwritten, not closed off: `Machine::connect_guest` is defaulted to
`Unsupported`, so HCS compiles untouched and a wire spawn on it is refused with a
sentence. hv_socket has the host→guest direction.
