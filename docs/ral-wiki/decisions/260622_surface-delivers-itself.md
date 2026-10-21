---
status: active
verified_at_commit: 61c288f
verified_at_date: 2026-10-21
anchors: [DeferredSink, DeferredSurface, CompletionPermit, DoneEvent, SessionEvent::DeferredSurface, Mooring::for_thread, InboxDeferred, Stamped::Surface, Avatar::arrive]
---

# A worker's `surface` is a second channel, scoped to the session

> Amended 2026-10-21:
> - **Terminal publication precedes deferred flush (`CompletionPermit`).**
>   Every worker thread owns a single `CompletionPermit`. On exit (normal,
>   failed, or panic via `Drop`), `CompletionPermit::finish` transitions the
>   handle to its terminal state (`Running` → `Ready`, `Stopping` → `Consumed`),
>   drains its byte buffers, and records the end `Stamp` *before* calling
>   `DeferredSurface::flush(DoneEvent)`, so a host woken by the deferred batch
>   can never observe the worker still non-terminal (`Running` or `Stopping`).
>   `FlushGuard` is deleted.
> - **One thread mooring constructor (`Mooring::for_thread`).** `Mooring::for_worker`
>   and `Mooring::for_stage_thread` are unified into `Mooring::for_thread(home,
>   surface)`.
> - **Redundant completion wakeups are filtered on arrival.** In exarch,
>   `Avatar::arrive` records the display/forensic completion cards from a
>   `Stamped::Surface` batch as usual, but adds the model-facing completion
>   notice to the exchange prompt/steering only if the worker is still unclaimed
>   (`ready` in the `` `workers `` probe) when the batch arrives — so a worker
>   already `await`ed, `race`d, or `cancel`led during the turn does not wake the
>   model a second time.

**`surface` speaks on two channels, told apart by the one thing a value's class
cannot carry — the *lifetime* of whoever receives it. A foreground run surfaces
on the *live* channel, scoped to its dispatch and ordered before its `Report`. A
detached worker outlives that dispatch, so it surfaces on the *deferred*
channel, scoped to the session: it buffers, and at completion delivers the batch
with a closing `` `done `` record, which the host renders at its next boundary.
One vocabulary and one decoder; two channels, because there are two scopes.**

Read as effects, a `surface` operation is handled by the innermost host handler
whose lifetime contains its producer: the dispatch for a run, the session for a
worker. A new *kind* of event is a class on the channel whose scope fits it; only
a new scope makes a new channel ([[design/engine-protocol|engine-protocol]],
[[related/asynchronous-effects|asynchronous-effects]]).

## Context

- `spawn` returns a handle and its worker runs on, past the `Report` of the run
  that spawned it ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).
- A worker must not hold the live sink: a card arriving after the `Report` would
  splice into sealed scrollback ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]],
  [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).
- Yet a worker's surfaced cards, and the fact of its completion, must reach the
  host without anyone polling for them.

## Decision

- **Two channels, one vocabulary.** The live channel is `Event::Surface`, inside
  a dispatch's window. The deferred channel is `SessionEvent::DeferredSurface`,
  outside any dispatch, received by the host's `DeferredSink`. Both carry
  `FOValue`s and both are decoded by the same function, so a card renders the
  same on either, only later on the second.
- **The worker buffers and delivers once.** Its mooring's surface is a
  `DeferredSurface`: a bounded buffer (an overflow marker past the cap) over the
  session's sink. A `FlushGuard` delivers the buffer plus a `DoneEvent`
  `{id, cmd, outcome}` on every exit — return, raise, or panic.
- **Delivery precedes the result.** The flush runs before the worker sends its
  result, so no eliminator observes a handle whose batch is undelivered.
  `await`, `race` and `poll` yield values and render nothing.
- **No sink, no surface.** A mooring without a deferred sink gives its workers
  no surface at all; what they would have surfaced is dropped.
- **Nesting inherits the scope.** `Mooring::for_worker` carries the session sink
  into every worker, so a nested `spawn` delivers at its own completion.
- **`watch` streams on the same channel.** Each line leaves as a one-value
  `` `watch `` delivery as it arrives — still deferred, still rendered at a
  boundary.

## Host side (exarch)

- `InboxDeferred` posts each batch to the root session's inbox as a
  `Stamped::Surface`, under a `Stamp` minted when the sink is built.
- A `/clear` bumps the inbox's epoch; the pop drops any post stamped before it,
  so a batch from a pre-`/clear` worker never renders.
- `Avatar::arrive` decodes the batch into the root scrollback exactly as a live
  run's values are, and the item wakes the model with the settled outcome —
  `errand #7 finished (exit 0) · … Await its handle for the value.`

## Safety

- **By type, not by rule.** A worker's mooring has no live sink and no desk
  (`Mooring::for_worker`); it cannot render now or enquire, because nothing it
  holds can.
- **A late card cannot splice.** The deferred channel renders only at a
  boundary, as fresh scrollback.
- **Data, not handles.** A batch is first-order, so it crosses the wire and
  lands in `events.jsonl` without special cases.

## Alternatives rejected

- **A control class on the live channel**, carrying the live handle to an
  exarch waiter thread
  ([[decisions/260622_surface-carries-control|surface-carries-control]]): a
  handle in the payload breaks first-order transport, and a second observer of
  the handle needs a shared settle.
- **Replay on `await`** into the awaiting run, arbitrated against the flush by a
  shared latch. The flush precedes the result, so the replay could never win,
  and winning would have bought only rendering one frame earlier at the cost of
  a cross-thread test-and-set.
- **Deferring the foreground too.** The foreground is the live frame; deferring
  it loses intra-turn progress.

## Where

- `core/src/types/mooring.rs` — `DeferredSink`, `Mooring::for_thread`.
- `core/src/types/handle.rs` — `CompletionPermit`; `core/src/types/handle/surface.rs` — `DeferredSurface`.
- `core/src/types/shell/workers.rs` — `DoneEvent`.
- `core/src/builtins/concurrency/birth.rs` — `spawn_child`, `CompletionPermit::finish` publishing terminal state before `DeferredSurface::flush`.
- `core/src/protocol.rs` — `SessionEvent::DeferredSurface`; carried by
  `core/src/carrier/wire/{serve,front}.rs`.
- `exarch/src/shell_eval.rs` — `InboxDeferred`; `exarch/src/agent/arrival.rs`
  — `Avatar::arrive`; `exarch/src/bus/post.rs` — the waking notice.
