# A worker ends with its children

**A gather ends with its children, and a worker's body is a gather.** A
*gather* — a `gather { … }` form, or a worker's body — owns every `spawn` or
`watch` born inside it, a pipeline stage's included: its `ScopeOwner`, in birth
order. When the gather begins exiting, it closes membership first so any late
birth from a pipeline stage thread still unwinding against that cradle is
cancelled and rejected before it can start. On exit it applies one rule,
`exit_brood`:

- **return**: membership is closed, then each child still unclaimed (`running`
  or `ready`) is claimed and awaited in birth order, exactly as `await` would
  ([[invariants/await-is-the-block-run-here|await-is-the-block-run-here]]):
  its bytes join the gather's own output, the first failure among them is the
  gather's, and the rest are cancelled and drained within the cleanup grace.
  *A normal return joins; it never cancels*;
- **halt** — a raise, an escape, a stop: membership is closed and every
  unclaimed child is claimed, cancelled and drained within the grace
  (`ScopeOwner::end`); a body that panics or unwinds closes membership and
  cancels them on `Drop` (`ScopeOwner::stop`) without waiting;
- **drain (I4′)**: on every normal or halting exit (`ScopeOwner::drain`), the
  gather waits, within one shared cleanup grace, for **every** child it
  accepted to reach a terminal state (`ready` or `consumed`) — including
  children cancelled earlier by `cancel $h` inside the body (`stopping`). So
  nothing a gather started outlives it by more than the grace.

Claiming is atomic, so a child claimed elsewhere first — an inner `await $h`,
a `race`, a `cancel $h` — is no longer the gather's to replay, and of that
claimer and the gather's join exactly one replays the child's bytes and meets
its ending. A child an outside party claimed and is still awaiting,
uncancelled, is left to it. A block that starts workers has not finished until
they have, so `await` of a worker is still its block run here, its last
implicit step included. A run outside any gather has no brood: its births
belong to the session.

**Cancellation is hereditary.** A gather's birth hangs under the scope current
when the gather was entered — a worker's own node inside a worker, the run's
scope at a `gather` in a run — and a session birth under the session root. So
`cancel $h` reaches everything `$h` started, and an interrupt or a wall on a
run reaches its gathers' children but never a session worker. A worker
converts a `Cancelled` or `TimedOut` stop owned by any node but the session
root into its own `worker was cancelled` / `worker timed out` error, so a
cancel made inside the session is a failure the awaiter can `try`, while a
stop the host owns passes as the escape it is. A gather that is not a worker
converts nothing.

See [[decisions/261020_gather|gather]] (I3–I5, I4′),
[[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]
(I6), [[internals/cancellation|cancellation]], and `docs/SPEC.md` §11.1,
§11.3.
