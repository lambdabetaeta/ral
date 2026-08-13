---
status: accepted
---

# `surface` delivers itself: finish the deferred half, drop the extra channel

**`surface` already has two halves. A foreground turn's surface renders live; a
detached worker's surface buffers and waits. The second half was never finished —
the buffer is delivered only if someone `await`s the handle, so a fire-and-forget
`spawn` strands whatever it surfaced. The fix adds no new channel concept and no
waiter thread: the deferred buffer delivers itself. At completion the worker flushes
it to a session-lived boundary sink — a real sink, but surface's own deferred
destination, carrying ordinary surface vocabulary — rendered at the next turn
boundary. One channel, one vocabulary, one decoder, two delivery regimes chosen by
lifetime.**

This supersedes both [[decisions/260622_surface-carries-control|surface-carries-control]]
and [[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]]. Both
solved the same problem — `spawn` should notify, not be polled — by adding
something beside `surface`: the first a non-rendering control event plus a waiter
thread, the second a separate `notify` channel. The capability they reach for
already half-exists in `DeferredSurface`. It only needs completing.

## Context

### surface already has two delivery regimes

`surface <value>` hands a `Value` to `shell.turn.surface` (`core/src/builtins/misc.rs:530`,
turn-local, [[decisions/260617_turn-local-state|turn-local-state]]). Core names no
meaning; exarch decodes by class — `AgentSink::emit` (`exarch/src/shell_eval.rs:71`)
turns an `io` `Map` into `Kind::Io` and a `` `card `` `Variant` into `Kind::Card`.
But the sink is installed two ways, keyed to the producer's lifetime:

- a foreground turn gets `AgentSink`, which decodes and emits to the live bus *now*
  (`exarch/src/shell_eval.rs:119`);
- a detached worker gets `DeferredSurface`, which only buffers into `surface_buf`
  (`core/src/builtins/concurrency.rs:132`, buffer at `:100`).

So the live/deferred split is not new. It is already there, and it exists for a
reason: a detached worker must not hold the live foreground emitter, or a late card
would splice into sealed scrollback ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]],
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).

### the deferred half is unfinished

A `DeferredSurface` only buffers. The buffer has exactly one way out: `await`/`race`
replay it into the awaiting turn (`replay_deferred_surface`, `concurrency.rs:333`,
draining the buffer at `:538`). Nothing else delivers it. So a `spawn` you never
`await` strands everything it surfaced — and the harness tells the model to poll
rather than await (`exarch/src/shell_eval.rs:165`), so the common path strands the
buffer. That is the whole defect: the buffer has no way to deliver itself.

### both prior ADRs add something beside surface

- [[decisions/260622_surface-carries-control|surface-carries-control]] surfaces a
  non-rendering `` `spawn-started `` control event in-turn, carrying the live
  handle; exarch registers it and runs a waiter thread that blocks on a new
  shell-free core settle and posts to the inbox. Cost: a control class, a waiter
  thread, a shell-free settle, and a two-observer cache handoff (its own named sole
  risk).
- [[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]] adds a
  second sink, `notify`, that the worker fires at completion. Cost: a second
  channel, a `spawn-done` compound event, and dedicated inbox/`Turn` variants.

Both costs are the price of treating post-turn delivery as a new thing. It is not.
It is the missing completion step of `DeferredSurface`.

## Decision

### one vocabulary, one decoder, two delivery regimes

`surface` stays the single language→host channel: typed `Value`, dispatched by
class. Pull the decode out of `AgentSink::emit` into a shared function, called by
both regimes:

- **live** — foreground turn: decode each value, emit to the bus now (`AgentSink`,
  unchanged);
- **deferred** — detached worker: buffer the raw values, and at completion deliver
  the batch to a boundary sink that renders them at the next turn boundary.

The *decoder* is shared. The *render dispatch* is not free: today cards reach the
rail only as `Kind::Card`/`Kind::Io` events on the bus, decoded in `AgentSink` and
grouped in `tui::handle` (`exarch/src/tui.rs`); the inbox/`Turn` path is text-only
(`Turn::Agent` re-emits a text `Kind::SubagentDone`, `tui.rs:2659`). So the boundary
regime needs a new `Turn` payload carrying the surfaced `Value`s and a `commit_turn`
arm that decodes them with the shared decoder and feeds them into the *existing*
`Kind::Card`/`Kind::Io` render path. The decoder is reused; the boundary path mints
the same `Kind` events the live path does, at the turn boundary instead of now. (This
is the tui-not-bus layering [[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]]
got right and this keeps; what is saved here is the *channel concept*, not a free
render.)

### `DeferredSurface` gains a destination

Today `DeferredSurface { buf }` is an orphaned buffer. Give it the session-lived
**boundary sink** the host supplies on the turn:

```
DeferredSurface { buf, boundary }   // boundary renders a batch as one fresh turn
```

Be honest about what this is: a real new sink — the same kind of session-lived
channel [[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]]'s
`notify` was, carried on `TurnRequest` beside its turn-local `surface` sink
(`core/src/driver.rs:227`). The saving here is not "no new sink";
it is no new *channel concept* (it is surface's deferred destination, carrying
surface vocabulary), no waiter thread, and no two-observer handoff. Because the
boundary sink lives on the worker's turn state, a nested `spawn` inherits it:
`spawn_child` installs the inner `DeferredSurface` with the same boundary, so the
inner buffer flushes at its own completion — resolving the degraded nested case
`surface-carries-control` recorded. The body's `surface`/`io` calls append to `buf`
as before. The boundary sink is exarch's, built over the inbox the async `agent`
already posts to (`exarch/src/tools/agent.rs:287`, `exarch/src/bus.rs`); it takes a
batch of surfaced values and renders them as one fresh turn.

### the worker flushes at completion

The flush wraps the whole worker body in an unwind-safe guard (as `pump` wraps
`work` in `catch_unwind`, `exarch/src/bus.rs`, and `run_child` does for agents,
`tools/agent.rs`), so it runs on **every** exit — clean return, raised `Err`, or
panic. On exit, core:

1. appends a final `done` value to `buf` — a small structural record carrying the
   label and the ok/err/panic outcome. Core names the event; exarch names its
   appearance ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).
2. flushes a **clone** of `buf`, stamped with the session generation, to `boundary`
   as one batch.

Two details are load-bearing, not incidental. *The clone:* the in-script eliminators
drain `surface_buf` into the handle's cache (`complete_handle`'s `mem::take`,
`concurrency.rs:538`) the first time any of `await`/`poll`/`race` settles the handle,
so the boundary must carry its own copy — otherwise a `poll` that settles first would
empty the buffer the boundary delivers. *The unwind guard:* the flush sits at the
worker's settle point (`concurrency.rs:145`) on the clean path, but a panicking body
unwinds before that point and drops `tx` without sending, so without the guard a
fire-and-forget `spawn` that panics would strand its buffer — the very defect this
ADR exists to fix, reappearing for crashing workers. The guard posts the partial
buffer and a panic `done` instead.

The detached worker's closure thus holds only its `DeferredSurface` — the buffer
(handle-owned) and the boundary sink (session-owned) — and never the live foreground
emitter. Completion is not a special channel; it is the worker's last surfaced event,
decoded by the same decoder as every other.

### `await` becomes pull-forward, not the delivery

`await`/`race` still replay the buffer — but now into the *current live turn*, as a
way to pull the cards forward when the model wants them in this frame. The two
regimes deliver to *different frames*: `await`/`race` render live in the awaiting
turn; the boundary flush renders a fresh turn later. So `await` is not an
optimization — it is the live-frame delivery, and the flush is the fallback for the
un-awaited case.

Deliver-once is a single `joined` *test-and-set* flag on the handle (folding today's
`surface_replayed`), shared by both renderers: whichever renders the batch first —
an eliminator (`await`/`race`, when it replays) or the boundary drain — sets the flag
and renders; the other sees it set and skips. So `await` before the boundary suppresses
the queued batch; the boundary before a later `await` suppresses the replay (the
`await` still returns its cached result record, just renders no cards). `poll` returns
the outcome as data but renders nothing, so it neither sets nor reads the flag; its
cards still surface at the boundary. The flag is a `Mutex<bool>`, not a bare `bool`,
because its two test-and-set sites can fall on different threads: a handle `await`ed
from inside *another* detached worker replays on that worker's thread, while the host
drains the same handle's flushed batch on its own — and a boundary that test-and-sets
inside `deliver` (rather than deferring it to a host-side drain, as exarch's inbox
does) sets the flag on the worker thread outright. The lock is the serialization:
whichever site acquires it first renders, the other finds it set and skips. For a
top-level handle whose eliminator runs inside a host turn this collapses to a
single-threaded check — the turn completes before the host drains the inbox — but the
lock, not that ordering, is what keeps the nested case correct. The batches never
collide either way: the worker flushed its own clone, so the boundary's copy is
independent of whatever the eliminators drained. There is no second observer racing
for the handle's cached *result* and no shared settle critical section — which is
exactly the two-observer cache handoff
[[decisions/260622_surface-carries-control|surface-carries-control]] had to pay for,
gone.

### the model-facing contract

As in both prior ADRs: don't poll; the host notifies. Concretely, the boundary turn
is model-facing in exactly the two-channel shape `Turn::Agent` already has: the `done`
event is the turn's *text* — it wakes the model with the labelled ok/err/panic notice,
so the model learns its `spawn` settled without polling — while the surfaced cards
render on the rail. (The model may then `await $h` for the value record; the replay is
suppressed by `joined`, so no card renders twice.) The poll-instructing timeout text
(`exarch/src/shell_eval.rs:165`) and the system prompt (`exarch/data/ral.md`) are
rewritten. `await`/`poll`/`race` remain — the in-script join and the on-demand pull —
but are no longer the cross-turn observation mechanism. This supersedes the poll idiom
of [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]].

## Safety

The detachment invariant — a detached worker holds only root/handle/session-owned
resources, never the foreground frame's emitter — holds by construction:

- the worker holds its `DeferredSurface`: the buffer (handle-owned) and the
  boundary sink (session-owned, the same class the async `agent` worker already
  holds, `tools/agent.rs:287`). It never holds the live `AgentSink`.
- the boundary sink can only render at a turn boundary, as a fresh turn. It cannot
  splice into sealed scrollback. The late-card hazard is a property of *live*
  delivery, and the worker has no access to it.
- this is the type, not a rule. The worker has no live-emit method to misuse —
  the safety `surface-carries-control` sought by withholding *every* channel is
  reached here by handing the worker *only the deferred regime*.
- live render from a still-running child stays foreclosed
  ([[decisions/260619_terminal-lease|terminal-lease]],
  [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]): the
  flush is at completion, never mid-flight.

## Why this shape

- **It finishes a mechanism instead of adding one.** `DeferredSurface` was always
  meant to be delivered; it had one delivery path (`await`) and no fallback. This
  adds the fallback and demotes `await` to an optimization.
- **It is the smallest change in concepts.** No control class, no second channel,
  no `spawn-done` envelope, no new `Turn` type. Completion is one surface class;
  delivery is one boundary sink; the decoder is shared.
- **It keeps one vocabulary.** Anything surfaceable in a foreground turn is
  surfaceable from a worker, rendered the same way, only later. A future event is a
  class on the one channel.
- **`events.jsonl` stays truthful with no special-casing.** Surfaced values are
  data, not live handles, so they serialise wherever they are delivered — unlike
  `surface-carries-control`, whose control event carried a handle and so had to be
  routed away from the log.

## What this supersedes

- From [[decisions/260622_surface-carries-control|surface-carries-control]]: the
  `` `spawn-started `` control class, the exarch waiter thread, the shell-free
  blocking settle, and the two-observer cache handoff all go away. There is no
  second observer of the handle — the worker delivers its own buffer, and `await`
  only pulls it forward.
- From [[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]]:
  `notify` as a separate channel, the `spawn-done` compound event, and the
  dedicated inbox/`Turn` variants all go away. The "second channel" is just the
  deferred regime's destination, carrying ordinary surface vocabulary.

What both got right, and this keeps: `surface` is the typed-`Value` language→host
channel dispatched by class; the detached worker must not hold the live emitter;
`spawn` notifies and the poll idiom retires.

## Alternatives considered

- **The two prior ADRs** (above) — superseded, not wrong: each was a correct way to
  reach the same end. This reaches it with fewer concepts, by completing a mechanism
  that already exists rather than adding one beside it.
- **Make the whole channel async** (defer the foreground too). Rejected: the
  foreground is the live frame, so deferring it loses intra-turn progress and
  re-mutes the live tabs [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]
  turned on. Async is strictly weaker; only the deferred half wants it.
- **A channel per purpose** (a third pipe tomorrow). Rejected: the channel count
  tracks the *delivery regime* (two: live, deferred), not the *purpose*. Every
  purpose is a class.

## Consequences

- One channel, one decoder, two delivery regimes. A new event is a class; a new
  lifetime does not arrive.
- `DeferredSurface` carries its destination on the turn state and flushes a clone at
  completion under an unwind-safe guard; `await`/`race` are the live-frame pull, the
  flush the un-awaited fallback.
- Core adds: a boundary sink on the worker's turn (a real new session-lived sink,
  threaded into nested workers), a `done` surface event, the `joined` fold of
  `surface_replayed`, and an unwind-safe flush guard. No waiter, no shell-free
  settle, no two-observer handoff.
- exarch adds: a boundary sink over the inbox; a `Turn` payload carrying the
  surfaced `Value`s; a `commit_turn` arm that decodes them with the shared decoder and
  feeds the *existing* `Kind::Card`/`Kind::Io` render path; and one render for the
  `done` class — core names the event, exarch names its appearance
  ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]). The
  decoder is shared; the render dispatch is the bus's `Kind` path, reached at the
  boundary.
- `/clear` must reject a stale boundary batch — and a `spawn` worker registers
  nowhere, so it has **no** `AgentRegistry` generation to borrow
  (`exarch/src/agent_registry.rs`; the async `agent` guards its inbox push with
  `registry.settle`, `tools/agent.rs`). Instead the boundary sink **captures the
  session generation when it is installed on the turn** — not when it flushes —
  exactly mirroring the async agent's register-at-birth / settle-at-completion: at
  completion it compares its captured generation against the live counter and *drops
  the batch rather than pushing it* if a `/clear` has since bumped the counter, while a
  batch already queued when `/clear` runs is dropped by the inbox clear `/clear`
  already performs. The two together — completion-time generation check for in-flight
  workers, inbox clear for queued batches — give the same coverage `agent` gets, and
  the session generation reuses `AgentRegistry`'s counter rather than minting a fresh
  epoch. Real host bookkeeping, but no new counter and smaller than
  `surface-carries-control`'s spawn registry.
- `events.jsonl` records the boundary batch only once it is minted as `Kind::Io`/
  `Kind::Card` (the rewiring above); given that, surfaced values are data, not live
  handles, so they serialise — unlike `surface-carries-control`'s handle-carrying
  control event.
- Deliver-once is a boundary-time flag check; a lost race yields at worst a
  duplicate notice, never a lost or doubled value.

## Open questions

- **`spawn` label.** The notice reads better with a label, but the mechanism does
  not require one (the worker can use the handle's `cmd`, which renders `<handle:…>`,
  `core/src/types/value.rs`). Whether to require `spawn "build" { … }` (mirroring
  `watch`, `concurrency.rs:225`) is a contract choice.
- **the session generation for `/clear`.** Resolved (see Consequences): the boundary
  sink reuses `AgentRegistry`'s generation counter — captured when installed on the
  turn, checked at completion before pushing — rather than minting a fresh epoch. That
  *some* generation guard is needed (a `spawn` worker has none today) was the settled
  part; reusing the existing counter is the chosen way to supply it.
- **live render from a running child.** Still foreclosed; unchanged
  ([[decisions/260619_terminal-lease|terminal-lease]],
  [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).

## See also

[[decisions/260622_surface-carries-control|surface-carries-control]] (superseded —
control class + waiter),
[[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]] (superseded
— notify channel),
[[decisions/260619_surface-carries-documents|surface-carries-documents]] and
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]] (the
vocabulary, and *operation* vs *appearance* — core names the event, exarch its
appearance),
[[decisions/260617_turn-local-state|turn-local-state]] (`surface` is turn-local; the
boundary sink is the session-lived destination the deferred regime flushes to),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the handle/detachment model and the poll idiom this refines),
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] (the
inbox, live-tab streaming, and the live-render open question this leaves foreclosed),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (the
detachment-holds-only-root/handle/session invariant),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]] (the
surfaced values are public `Value`s, not core's repr),
[[map/exarch/shell-eval|shell-eval]] (the `AgentSink`/decoder seam),
[[map/exarch/cards|cards]] (the render subsystem),
[[map/exarch/tools|tools]] (the inbox path the boundary sink reuses),
[[map/core/builtins|map: builtins]] (`spawn` and the concurrency primitives), and
`docs/SPEC.md` §11.2 (the handle settle/replay rule — `await` as pull-forward leaves
it intact).
