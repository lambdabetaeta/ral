---
status: active
---

# Synod's exchange ends at fleet quiescence

**Synod's after-checkpoint and report refresh happen only when the trunk is
parked, no live children remain, and their results are drained — never at the
first moment the trunk itself falls quiet.** This is a deliberate departure
from exarch's chat-while-they-work model: exarch's trunk may converse with a
human while a helper is still running, because nothing there promises the
transcript is the whole story. Synod's product promise — "a checkpoint before,
a report after, anything can be put back" — is not compatible with a report
written while a helper is still writing to the folder it describes.

This supersedes the "never a fleet" half of the old `fuel: 0` comment at
`synod/src/session.rs`: synod's trunk no longer refuses every `agent-start`.
The [[design/engine-protocol|engine protocol]] left the shape
of a wire-side spawn fixed but deliberately unbuilt "until it has a caller" —
synod is now that caller, delegating to helpers that run concurrently inside
the same guest, against the same folder, under the same safety net. Synod's
trunk carries `SPAWN_FUEL` (3) in place of `fuel: 0`, the same depth budget
exarch's own trunks carry — spawning is universal, bounded by depth and not by
fan-out, exactly the law [[design/agents|agents]] states with no seat-type
asterisk.

## The law

The after-checkpoint and the report refresh happen only when:

1. the trunk is parked (nothing left for it to do this exchange), **and**
2. no live children remain, **and**
3. their results are drained and announced.

`exarch::headless::converse_settled` is the driver this law is built into: it
runs the ordinary `attend` loop, not `attend_backlog`, under a park policy that
answers `HeldByChildren` — not `Held` — while children are live, and only
treats the trunk as settled once the fleet is childless and empty. The
`park_mode` semantics that decide focus and cancellation for exarch's own TUI
do not move; the driver is an addition beside them, not a change to them.
`dev/docs/VM/EXARCH-VM-v2.md` states the same law at the workspace-export
grain ("at a turn boundary the engine stops admitting work and obtains a
workspace barrier"); this is that law at the exchange grain.

Every wait this law imposes ends on its own: `MAX_STEPS` bounds a runaway
turn, a returning child that finishes without `reply` is re-nudged then fails
honestly, and a child parked `HeldByChildren` terminates by induction on its
own subtree. The two `ParkMode` variants that do *not* end by themselves —
`Engaged` (needs a human `steer`, which synod's UI never sends) and
`UntilCancelled` (needs an armed self-schedule) — are refused at
`converse_settled`'s own construction for a synod trunk, the same class of
refusal a fuelled wire trunk without a hatchery gets. So the fleet the driver
waits on always quiesces; the one case the wait rule does not catch is two
helpers `message`-ing each other in a loop, each turn real work and fuel
bounding depth, not turn count — recorded as an open risk, not solved by this
law, and the persona's to discourage.

## Consequences

- Synod's `fuel: 0` becomes `SPAWN_FUEL`, and the comment at
  `synod/src/session.rs` that justified id-blind sink routing by "no
  sub-agents exist" is retired along with the fuel figure that made it true.
- `Conversation::exchange` drives `converse_settled` instead of
  `converse_sink`; the after-checkpoint's position in `exchange` does not
  move — this law is what makes that position correct rather than
  coincidental.
- The status bar gains a `WaitingOnAgents` state, read off the same
  `HeldByChildren` park verdict the driver computes — a synod window that
  never had a fleet before now has something honest to say while one runs.
- A long exchange has no user-side brake yet: the law holds the exchange open
  as long as helpers genuinely work, and whether synod wants a "wrap up now"
  gesture is a product question for after the first real transcripts, not
  machinery to presuppose here.

## Amendment — "live" reads "busy"

Since [[decisions/260826_reply-parks|reply-parks]] a child that has replied
stays registered, parked under its idle lease. Clause 2 therefore reads "no
*busy* children remain": the exchange ends once every child has replied or
died, and the repliers linger, fetchable by `` agents `reply ``.

## See also

[[design/engine-protocol|engine-protocol]] (the rendezvous
shape this law's callers use, and the paragraph this ADR's caller now names),
[[design/agents|agents]] (spawning is universal, bounded by fuel — the law
synod's `SPAWN_FUEL` now carries with no seat-type exception),
[[map/synod|synod]] (the helper surface, the hatchery, the sink routing this
law drives), [[map/exarch/agent|agent]] (`ParkMode` and the `attend` loop this
driver reuses).
