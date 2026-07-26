---
status: superseded
superseded_by: decisions/260726_cancel-is-a-join
---

# A published cancel flag is leaked, because a signal handler's load and its dereference are two instants

> Superseded by [[decisions/260726_cancel-is-a-join|cancel-is-a-join]], which
> takes the two-flag alternative argued below: there is no published pointer
> left to keep alive, so the leak, the guards, and the save/restore go with
> it. The race analysis and the measurements here are what that decision rests
> on.

**The signal-reachable cancel slots publish a *borrowed* pointer into a run
frame's cancel flag, and `request` reads that slot in two steps — load, then
dereference. A process-directed signal is delivered to an arbitrary unblocked
thread, so those two steps can straddle the death of the scope the pointer
names: a use-after-free reachable from a signal handler. `publish` now leaks
one strong share of the scope's `Arc`, making every published flag immortal.**
This closes the memory-safety hole today; it is deliberately *not* the last
word on the design (see *Live alternatives*).

## The race

`request(slot, cause)` (`core/src/process/cancel.rs`) is:

```rust
let p = slot.load(Ordering::Acquire);
if !p.is_null() { unsafe { (*p).fetch_max(cause as u8, Ordering::Release) } }
```

- The pointer names an `AtomicU8` living inside a `ScopeNode`'s `Arc`, owned by
  the run frame `RunGuard` installed. When the guard drops it restores the
  slot's predecessor and the frame's scope is freed.
- **A handler can load `p` before the restore and dereference it after the
  `Arc` is gone.** Restoring the slot recalls nothing a handler already holds
  in a register.
- **One publisher is enough.** The second party to the race is the signal, not
  a second publisher. The prior SAFETY note — "the publishing guard restores
  the prior pointer before that scope can drop" — is a claim about *ordering on
  the publishing thread*, applied to a race whose other participant runs
  asynchronously on whichever thread the kernel picked. So is `RunGuard`'s
  field-declaration-order argument: correct about its own drop sequence,
  silent about the handler.
- The shipped invariant "at most one session per process publishes"
  ([[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]) therefore
  does not close it either — and production does not hold that invariant
  literally in any case: the REPL runs a second publishing session, the hook
  shell minted by `Shell::child_from` (`ral/src/repl/plugin.rs`), alongside the
  main session whose prompt render dispatches its own run
  (`ral/src/repl/prompt.rs`). Both are on the REPL thread, so they nest; both
  publish.
- The observed symptom is the freed flag byte re-read as a live cause — a
  spurious `cancelled` at status 130 out of a run nobody interrupted.

## The fix

```rust
fn publish(slot: &'static AtomicPtr<AtomicU8>, scope: &CancelScope) -> CancelSlot {
    std::mem::forget(scope.0.clone());
    ...
}
```

- **What it buys.** No published flag is ever freed, so the handler's
  dereference always lands on live memory. It also removes the stale-cause
  symptom outright: a retired run's leaked flag is polled by nobody.
- **What it costs.** One scope node — 32 bytes on a 64-bit host — per
  publishing run, held for the life of the process. Re-publishing a scope that
  is already immortal (the session's durable root, published every run) leaks a
  reference count, not an allocation.
- **What it does not fix.** With two threads publishing concurrently, the blind
  save/restore can leave a slot aimed at a finished run, so a cancellation is
  *dropped* rather than delivered. That is a missed cancellation, never memory
  corruption.
- This is the same move exarch's `publish` already makes for its per-exchange
  `Token` (`exarch/src/agent/cancel.rs`), for the same reason. The two modules
  now read as one idea.

## Live alternatives

The leak keeps the pointer and pays to make it safe. Three designs remove or
re-shape the pointer instead; none is foreclosed by this change.

- **Two process-lifetime flags** — one "foreground interrupted", one "session
  terminated", both `static`, written by the handler and drained by the run
  door at its poll points. Nothing is published, nothing is restored, nothing
  dangles. Its case does not rest on the dropped-signal window: it *deletes the
  bookkeeping that produced the bug*, and with it the leak, the guards, and the
  save/restore discipline. The open questions are cross-run staleness (a flag
  set for a run that has already ended must not cancel the next one) and
  whether the drain points cover every reader — `foreground_cancel_cause`, the
  parked enquiry desk, `process::check`.
- **An epoch-tagged `'static` cell slab** — publications hand out an index plus
  an epoch tag rather than a pointer; a stale handler write is rejected by tag
  mismatch. Fixes the dropped-signal case as well, at the price of a new
  allocator and a new lifetime discipline.
- **Inverted publication** — the handler writes a cause into a fixed location
  and the run door drains it at poll points; a degenerate case of the two-flag
  design with a `CancelCause` instead of a bool.

The slab and the inversion are argued from the dropped-signal window, which
production does not reach today and — as the evidence below shows — is
reproducible the instant a second thread publishes. The two-flag design is
argued from something else entirely, and does not need that window: it deletes
the pointer.

## Evidence: what concurrent publishers actually do

Measured on this change, in `ral-core`'s test binary, with the leak in place
(so none of it is memory corruption):

- **Read-side cross-talk is real and frequent.** Letting every test session
  publish (`SessionState::publishes_signal_slots` unconditionally `true`) fails
  the binary in roughly two runs in three, at default and at 16 threads:
  `engine::wire_desk_tests`' parked enquiry reads `foreground_cancel_cause()`
  and sees *another* test's cancelled run, raising a spurious `cancelled` at
  status 130. The park (`core/src/engine.rs`) is the only production reader of
  the slot.
- **Write-side dropped cancellation is real too.** Removing the serialisation
  between the tests that *do* publish makes
  `engine::engine_session_tests::cancel_settles_an_in_flight_run_promptly` wait
  out its 20 s ceiling: `request_foreground_cancel` landed on a foreign run and
  the intended one never heard it.

So the dropped-signal case is not theoretical: it reproduces the moment a
second thread publishes, and stays out of production only because nothing
there does. Any design that keeps a published pointer must keep that
invariant; the two-flag design has no pointer to mistarget.

## Consequences

- `SessionState::publishes_signal_slots` keeps its `!cfg!(test)` default. The
  memory-safety reason for it is gone, but the cross-talk reason is not: test
  sessions run concurrently, and a published slot is process-global.
- `SLOT_SERIAL` and the poison-tolerant test lock stay, for the same reason.
- `RunGuard`'s slot guards no longer carry a memory-safety obligation; they
  bound only *when* a slot fires.

See also [[internals/cancellation|cancellation]],
[[decisions/260706_signals-are-causes|signals-are-causes]] (the translation of
signals into causes on these slots),
[[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]] (the
one-publishing-session rule, which this shows to be a *delivery* invariant and
not a safety one), and [[map/core/io-process|io-process]].
