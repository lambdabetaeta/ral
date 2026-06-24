---
status: active
---

# Esc drives a non-escalating cooperative interrupt

A frontend in raw mode owns its own task-level cancel, and **a single Esc
must return to the prompt without ever escalating ral's termination counter
toward force-exit.** ral's `SIGNAL_COUNT` is an *escalating* counter — a real
SIGINT / SIGTERM / SIGHUP `fetch_add`s it and the third occurrence forces
`libc::_exit` — the right policy for a wedged interactive shell, the wrong one
for a frontend that drives a per-turn cancel and returns on the first press.

## The missing operation

`process::check` treats any count `>= 1` as interrupted, so unwinding the
evaluator needs the counter at exactly **1**, never a `fetch_add`. The frontend
wants the counter's *effect* (unwind at the next poll) with the cancel-scope's
*discipline* (no escalation). That operation is `process::interrupt` — a
non-escalating `SIGNAL_COUNT.store(1)`, idempotent: it can fire any number of
times and never reach the third-signal `_exit`. It sits beside `clear`, distinct
from a real signal (which escalates) and from a
[[decisions/260504_hot-path-cancellation|`CancelScope`]] (a per-subtree flag).

## The delivery path

Raw mode disables `ISIG`, so a keypress no longer makes the kernel SIGINT the
foreground job; the frontend recreates that delivery in two halves:

- a foreground **external child** (its pgid differs from ours) still takes a real
  SIGINT via `process::interrupt_foreground_child` — signalling *another* group
  carries no escalation, and the child's death lets the evaluator's blocking
  `wait` return;
- **ral itself** is unwound by `process::interrupt`, not by forwarding a
  synthetic signal into the escalating handler.

So repeated Esc can never bump the counter past 1.

## What is preserved

- A genuine external `kill -INT` still escalates — the real-signal handler is
  untouched — so the force-kill failsafe survives for a truly wedged process.
- [[invariants/turn-ends-ready|turn-ends-ready]] holds: an interrupt in ral is a
  *value* (`Break::Error`) threaded home by the trampoline, not a `panic!`, so
  every capability-frame pop, sink restore, and child cancel-scope teardown
  completes on the way out — a cancel leaves shell state intact.

## Companion: the worker phases

The same effort makes the worker's silent synchronous phases legible and the
late ones cancellable. A transient `Kind::Phase` label — rendering context /
waiting for model / typechecking / compacting — shows beside the spinner and is
recorded to the headless `events.json`, so a wedge names itself instead of
animating a bare dot. `compact` bails before its summarize request on a
turn-boundary cancel rather than issuing a round-trip only to abort it. Which
remaining phase, if any, actually wedges is left to measurement: the phase labels
and the debug `dbg_trace!` timing are the instrument, not a speculative rewrite.

Maps: [[map/core/io-process|io-process]], [[map/exarch/frontend|frontend]],
[[map/exarch/agent|agent]].
