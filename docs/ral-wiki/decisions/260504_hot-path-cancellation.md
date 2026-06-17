---
status: active
---

# Cooperative cancellation in hot loops

ral cancels cooperatively — no signal preemption. `process::check(shell)`
(`core/src/process/signal.rs`) returns `Break::Error` with status 130 when a
queued SIGINT (`SIGNAL_COUNT`) or the live `CancelScope` (`shell.local.cancel`,
walked to its root) is set; a long computation stays responsive only where it is
polled. exarch's per-call watchdog cancels exactly that scope
([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] is the Esc/SIGINT
half), so whether a tool call honours its timeout is the same question as whether
the running code polls.

## What polls

`process::check` runs at the **evaluator's statement boundaries** — `eval_bind`,
`eval_seq`, `eval_chain` (`evaluator/comp.rs`), and the trampoline's `Control::Tail`
arm (`evaluator/trampoline.rs`) — plus the pipeline driver (`runtime/pipeline.rs`).
So a recursive ral function (each self-call is a trampolined tail call) and any
sequence or `let`-threaded body are interruptible by construction.

## The gap

A loop that builds its result without re-entering one of those boundaries does not
poll. Two cases:

- **Pure-Rust loops.** `range` was a bare `while i < end { out.push(…) }` — and
  took no `shell` to poll with.
- **Higher-order combinators over a value-bodied callback.** `map` / `filter` /
  `fold` / `each` / `sort-list-by` iterate in Rust and reach a checkpoint only
  through the per-element `apply`. A callback whose body is a bare value —
  `{ |h| equal $h[file] $f }`, `{ |x| $x }` — produces that value with no bind,
  no sequence, and no trampolined tail call, so its `apply` polls nothing. The
  combinator then runs to completion regardless of a pending cancel. (The earlier
  reading that these poll *incidentally* via the trampoline held only for callbacks
  that happen to re-enter a boundary; it is not a property of the combinator.)

The trigger was an incident: `grep-files '\.expect\(|\.unwrap\(\)'` over the repo
spent ~10 minutes in a per-file `filter` rescanning every hit (the quadratic shape
fixed separately in the `grep-files` prelude helper), and the watchdog could not
cut it short because that `filter`'s value-bodied predicate never polled.

ral's `List` is `imbl::Vector`, so `[...$acc, x]` is an O(1) path-copy append, not
an O(n) clone. Chunked polling therefore belongs on numeric tight loops like
`range`, not on list construction, and no bulk-clone helper is needed.

## The decision

Poll explicitly inside the collection builtins (`builtins/collections.rs`) rather
than rely on a callback re-entering the evaluator:

- `each` / `map` / `filter` / `fold` / `sort-list-by` poll at the top of each
  element — negligible beside the `apply` that follows.
- `range` takes a `shell` and polls every `INTERRUPT_POLL_CHUNK` (1024) steps,
  chunked so a span shorter than the chunk pays nothing.

The cooperative model is preserved: no preemption, status 130 on interrupt, no
attempt to fix quadratic *user* algorithms, and no blanket sweep of every
string/bytes loop in the same change. Off the cancellation path the loops are
byte-for-byte as before — `check` returns `Ok` and falls through.

## Covered

`builtins::collections::tests` — each combinator driven directly under a
pre-cancelled scope with a value-bodied lambda (so the loop's own poll is the only
reachable checkpoint) aborts at status 130; a long `range` trips past the chunk
while a short one completes untouched. The negative was checked: removing the
`map` and `range` polls fails exactly those two tests.

See also [[map/core/evaluator|evaluator]], [[map/core/builtins|builtins]],
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] (the exarch-side
token the watchdog and Esc raise), and [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]].
