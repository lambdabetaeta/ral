---
status: active
---

# `poll`'s `` `pending `` arm carries partial output — a cumulative peek, not a delta

**A `poll` of a still-running handle returns `` `pending `` `{stdout, stderr}`, the
bytes the worker has written *so far*, cloned non-destructively from its live
buffers. The snapshot is cumulative (a growing prefix), never a drain-on-poll
delta — so the "bytes leave a buffer exactly once" invariant survives, at the price
of making pending polls non-idempotent.**

## What changed

[[decisions/260615_poll-total-failed-arm|The settle decision]] gave `poll` a
`` `pending `` arm with a `Unit` payload: while the worker ran, `poll` could report
*that* it was running but nothing it had done. Yet a spawned worker's stdout/stderr
already accumulate in shared, mutex-guarded, already-bounded `ByteBuffer`s
(`HandleInner.stdout_buf`/`stderr_buf`) as it runs — they are only emptied once, at
completion, by `take_buffer` in `complete_handle`. Partial `poll` reads those
buffers rather than plumbing any new capture:

- `core/src/io/sink.rs` gains `peek_buffer` — clone the buffer under the lock, the
  non-destructive peer of `take_buffer`'s `mem::take`.
- `builtin_poll`'s `` None `` (pending) arm returns
  `{stdout: peek_buffer(…), stderr: peek_buffer(…)}` instead of `Unit`
  (`core/src/builtins/concurrency.rs`).
- `poll_variant`'s pending arm widens from `Ty::Unit` to
  `{stdout: Bytes, stderr: Bytes}` (`core/src/typecheck/builtins.rs`).

## The decision that matters: cumulative, not delta

The snapshot is a cumulative peek (clone a growing prefix), **not** a
drain-on-poll delta (take what's new since the last poll). Draining on `poll`
would:

- reintroduce the peek-vs-drain split that
  [[decisions/260615_poll-total-failed-arm|260615]] explicitly closed — there would
  again be two places bytes leave the buffers, on different code paths; and
- break the `CompletedHandle` cache: `complete_handle` `take`s the *whole* buffer
  once and every eliminator projects that one cached value, so a `poll` that had
  already consumed a prefix would leave `await` with a truncated record.

With cumulative peeks the invariant **bytes leave the buffers exactly once** is
preserved: settle still `take`s everything; partial polls only *clone*. A pending
poll can never steal bytes a later `await` or `` `settled `` poll must still see.

## The cost: a deliberately re-scoped invariant

260615 stated that "repeated observations are consistent by construction." Partial
poll re-scopes that: it holds for **settled** observations (still projected from the
one cached `CompletedHandle`), but **pending** polls become non-idempotent — each
reports monotonically more output as the worker writes. That is inherent to
"partial": a snapshot of a moving target cannot also be stable. The guarantee is now
precisely "settled observations are stable across repeats," and this ADR is the
record of that reversal.

## Two benign wrinkles

- **Watched handles report empty.** A `watch` worker's bytes flow live through
  `Sink::LineFramed`; its `stdout_buf`/`stderr_buf` stay empty by design. A pending
  `poll` on a watch handle therefore reports empty output — documented, not an error.
- **Bounded, and byte-clean.** `SINK_BUFFER_CAP` (16 MiB, with its one-shot
  truncation marker) already bounds a chatty server, so peeking a running worker is
  memory-safe. Returning `Bytes` (not `String`) sidesteps mid-codepoint boundaries
  in a partial snapshot, exactly as the settled arm does.

## Why it's worth it

`output-capture-and-detachment.md` flagged the limitation: a spawned server "never
settles, so `poll $h` reports `` `pending `` without draining the buffer… bytes
observable only once it settles," and `watch` — the only primitive that streams a
running worker — is REPL-only, because exarch's per-call capture sinks cannot host a
root-surviving writer ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]).
Partial `poll` gives exarch a *pull-based* way to read a running worker's
accumulated output — the headless counterpart to `watch`. It turns the old
"fire-and-`cancel`, or redirect to a file" workaround into a first-class read.

See also [[decisions/260615_poll-total-failed-arm|the settle decision]],
[[internals/output-capture-and-detachment|output capture and detachment]],
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]],
[[map/core/builtins|map: builtins]]; `docs/SPEC.md` §11.2.
