---
status: active
---

# Signals are causes: one delivery mechanism, one escalation ladder

**A delivered termination signal is translated, at the handler, into a
[`CancelCause`] on the published cancel slots — the same cooperative delivery
every other cancellation already uses — and the old counter survives only as
the force-exit ladder it always secretly was.** SIGINT interrupts the
foreground turn; SIGTERM/SIGHUP terminate the durable root with a new cause,
`Terminate`, at exit 143. `process::check` polls only the scope tree. There is
no longer a second, parallel signal-delivery mechanism for a wait loop to
forget to consult.

> The *substrate* this rides on — the published `AtomicPtr` slots — is
> superseded by [[decisions/260726_cancel-is-a-join|cancel-is-a-join]]: a
> handler now contributes a cause to two process-lifetime cells that facing
> scopes fold. The translation thesis is untouched, the rejection of
> session-lifetime root publication below is dissolved (nothing is published),
> and the Residual's idle-SIGTERM case is closed — the root cell is never
> spent, so an idle delivery latches and the REPL reads it at its next prompt
> boundary.

This closes issue #3 of the 2026-07-05 core audit: *signals can't preempt a
blocked standalone external; SIGTERM never cancels*. The audit deferred the
issue believing the fix needed a new signal→wait-loop bridge (a self-pipe /
eventfd every wait selects on). It didn't: the bridge already existed.

## The hole

Batch mode's `handler` (SIGINT/SIGTERM/SIGHUP) bumped `SIGNAL_COUNT` and
forwarded to the pipeline relay slots — nothing else. `SIGNAL_COUNT` was read
by `process::check` at the evaluator's poll points, but `RunningChild::wait`'s
poll loop watched only its `CancelScope`. So with the evaluator parked inside
a standalone external's wait:

- **Ctrl-C / SIGINT in batch** left ral blocked and the child running; the
  third signal `_exit(130)`ed ral and orphaned the child alive (a
  `NewSession` child gets no terminal signal from the kernel either).
- **SIGTERM / SIGHUP** were worse: `term_handler` cancelled no scope in *any*
  mode, so a `timeout(1)`- or systemd-style SIGTERM could not stop a running
  external at all.

Meanwhile the substrate for the fix was already shipped:
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]'s
signal-reachable slots (`request_foreground_cancel` / `request_root_cancel` —
a lock-free slot load plus `fetch_max`, async-signal-safe by construction),
published every turn by the turn doors, and already driven by the interactive
SIGINT relay and the SIGQUIT root-abort. Only the batch/term handler never
used them. And the wait loop was never blocked in a syscall — it polls its
scope at 5–100 ms backoff. Nothing selects on an fd; nothing needed to.

## The decision

Four moves, each deleting a parallelism rather than adding a mechanism:

1. **Translate at the handler.** `handler` now delivers SIGINT as
   `request_foreground_cancel(Interrupt)` (exactly what the relay does) and
   SIGTERM/SIGHUP as `request_root_cancel(Terminate)`. A shutdown request
   lands on the *root* — detached workers are part of the session being shut
   down — and its externals are torn down SIGTERM-first by
   `terminate_group`'s existing ladder, so a well-behaved tree receives the
   very signal the supervisor sent ral. Windows' console-control handler
   makes the same foreground-`Interrupt` translation.

2. **`Terminate` joins the cause order** — `Interrupt < Explicit < Deadline <
   Terminate < RootAbort` — with message `"terminated"` and exit **143**
   (`128 + SIGTERM`). The cause→message/exit vocabulary moves onto
   `CancelCause::message` / `CancelCause::exit_code`, deleting the duplicate
   match in the engine's enquiry desk.

3. **`check` is scope-only; the counter is demoted to `ESCALATION`.** The
   ladder's one job is the third-delivery `_exit(128 + sig)` — the last
   resort for a process whose cooperative delivery is wedged, which is
   precisely why it must stay independent of the thing it backstops.
   `clear()` resets it at acknowledgment boundaries; `escalation_pending()`
   is an observability probe; `interrupt()` / `is_interrupted()` are deleted
   (delivery goes through the scopes, as
   [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
   already did for Esc).

4. **One wait loop.** `RunningChild::wait` no longer skips the cancel-aware
   poll when `park_on_stop` is set — parking is a *classification* of a
   stopped child, not a reason to block in `waitpid` with no poll. This also
   closes a hole the audit didn't list: the blocking path was
   SA_RESTART-proofed against signals, so SIGTERM couldn't preempt even an
   interactive foreground external (vim under the REPL).

One consequence made visible by the fix: a root cancel is one-way, so the
REPL loop now observes the durable root at the prompt boundary and exits with
the cause's code — a SIGTERM'd REPL exits 143, and a Ctrl-\ root abort ends
the session instead of dealing an unusable prompt whose every turn says
"aborted".

## What was rejected

- **The self-pipe / eventfd.** The audit's original Diamond sketch. It buys
  sub-100 ms cancel latency over the existing poll loop at the price of a
  per-platform fd-wait design (pidfd, kqueue `EVFILT_PROC`, Windows handles).
  The conceptual unification — signals translated once, into the one cancel
  tree — does not need it, and the poll loop is the mechanism every exarch
  tool-timeout kill already trusts.
- **Session-lifetime root-slot publication.** Publishing the durable root at
  boot (instead of per-turn) would let an idle-time SIGTERM latch. But the
  slot guards' save/restore discipline is LIFO on one thread
  ([[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]);
  construction-time publication would put every concurrently-constructed
  `Shell` (every test) into the slots and turn out-of-order drops into a
  dangling flag pointer. Per-turn publication stays.

## Residual

A signal landing when no turn is published — the batch parse window
(milliseconds), or a REPL parked in its line-editor read — reaches only the
ladder: cooperative delivery has nowhere to land, and `SA_RESTART` means the
read does not return. Three deliveries still `_exit`. Making a *single* idle
SIGTERM end the REPL read would need the frontend's read loop to observe the
root; deliberately out of scope here.

## See also

- [[internals/cancellation|cancellation]] — the updated map.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] — the slots
  this rides on.
- [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
  — the same discipline for the user-facing interrupt.
- `core/src/process/signal.rs`, `core/src/process/signal/unix.rs`,
  `core/src/runtime/command/child.rs`, `ral/src/repl/session.rs`.
