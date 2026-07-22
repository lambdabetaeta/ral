---
status: active
---

# The engine's read deadline is armed by the first ping

**A wire engine starts with infinite patience and arms a read deadline only
when the front-end first pings — a front-end that pings has promised to keep
pinging, and one that never pings is left to wait forever.** Liveness is not
negotiated at `Attach` and not imposed on every session; the first
`Frame::Ping` is the whole promise, and its absence is a valid contract too.

## Context

The engine speaks one frame protocol down two transports whose deaths look
nothing alike. A same-host child on a socketpair dies as an **EOF**: the kernel
closes the far end the instant the front-end process goes, and the engine's
next read returns at once. A guest across a virtual socket dies as
**silence**: nothing closes, no EOF arrives, the stream simply stops carrying
frames. The law that covers both (`Frame::Ping`) is that any received frame is
proof of life, and heartbeats exist only to manufacture traffic so that silence
can mean nothing but death. But only the silent transport needs them — the
socketpair front-ends, the REPL and exarch today, never ping, because their
death is already told to the engine as EOF.

So the engine cannot hold one read deadline over every session. A deadline
armed unconditionally would time the REPL out the moment it sat idle at a
prompt, and would oblige every socketpair front-end to grow a heartbeat ticker
it has no other reason to own.

## The decision

The engine arms its read deadline on the **first `Frame::Ping` it receives**,
and not before. Until then it blocks on reads with no timeout — the socketpair
case, whose EOF it will see without help. Once a ping has arrived the front-end
has declared itself a pinging front-end, and a stretch of silence longer than
`HOST_SILENCE_DEADLINE` (30s — six of the host's default 5s intervals, so no
scheduling jitter can fake a death) can only be that front-end's death: the
engine cancels the in-flight turn, waits bounded for the worker to settle it,
and exits. On the guest that exit is the daemon's cue to power the machine off,
exactly the recorded no-restart policy.

The promise is asymmetric by design. A front-end whose failure mode is silence
*must* ping, and by pinging arms the deadline that catches its own death. A
front-end whose failure mode is EOF need not, and by never pinging keeps the
engine's patience infinite — correctly, because its death needs no deadline to
be noticed.

## Alternatives rejected

- **An unconditional read deadline.** Every session gets a timeout whether or
  not a heartbeat feeds it. This forces the REPL and exarch — same-host
  front-ends that idle at a prompt indefinitely and die as EOF — to manufacture
  ping traffic for the sole purpose of not tripping a deadline they never
  needed. Their death is already immediate; the deadline would be pure ceremony
  on the transports that do not want it.
- **A liveness field carried on `Attach`.** The front-end could declare "I will
  ping" in the handshake and the engine arm its deadline from that flag. But the
  first ping *is* that declaration, and it proves what a flag only promises: a
  front-end that claims it will ping and then cannot is caught by exactly the
  silence a front-end that never claimed to would escape. It is negotiation
  ceremony for a fact the traffic already carries.

## Consequences

- A front-end that begins pinging enters a contract it cannot silently leave:
  once the engine's deadline is armed, ceasing to ping is indistinguishable from
  death and will be read as death. This is the intended shape — the adopted
  transport's ticker pings for the life of the connection, at a cadence
  comfortably shorter than the deadline so a single dropped exchange never
  condemns a live peer. The ticker starts only when `attach` has put the
  `Attach` frame on the wire, so no ping can precede the handshake — by
  construction, not by the front-end usually winning a race.
- `PROTOCOL_VERSION` moves to 3: `Frame::Ping`/`Frame::Pong` are new frames on
  the envelope, and an envelope change is a protocol change, refused loudly at
  `Attach` against a stale peer.
- The settle-then-exit durability the deadline triggers now also covers the EOF
  and read-error teardowns: whatever ends the reader loop, a running turn is
  cancelled and reaped before the engine goes, so nothing it spawned outlives
  it.

## See also

[[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]]
(the two transports and the disconnect-mid-lease corner this refines),
[[decisions/260706_enquiry-channel|enquiry-channel]] (law §6.4, liveness
detected not assumed — this is its mechanism),
[[map/synod|synod]] (the product whose guest is the silent transport).
