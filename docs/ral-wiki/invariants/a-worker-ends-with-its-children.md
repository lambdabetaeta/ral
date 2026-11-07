# A worker ends with its children

**A gather ends with its children, and a worker's body is a gather.** A
*gather* — a `gather { … }` form, or a worker's body — owns every `spawn` or
`watch` born inside it, a pipeline stage's included: its `BroodGuard`, in birth
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
  (`BroodGuard::end`); a body that panics or unwinds closes membership and
  cancels them on `Drop` (`BroodGuard::stop`) without waiting;
- **drain (I4′)**: on every normal or halting exit (`BroodGuard::drain`), the
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

**Lifetime and failure are orthogonal.** The owner determines both policies,
not one hidden success obligation for every worker:

| Work | Failure obligation | Lifetime obligation |
|---|---|---|
| Brood: a gather's child, a worker's included | yes (I4) | owner joins on return; cancels and drains on halt |
| Loose session `spawn`/`watch` | none; only explicit `await`, `race` or `poll` observes its ending | a returning last run waits and replays bytes; session end takes it down on halt |
| External `detach` | none | outlives the session |

`Run.last`'s wait claims still-unclaimed session workers in birth order,
writes stdout then stderr and drops every ending, even a panic. The script's
value and status stay its own. A failure or stop while waiting or replaying
can halt that wait; a worker's ending cannot. REPL quit and exarch session end
are halts. A forgotten `spawn { serve }` can hold a returning script open:
`cancel` work no longer needed, halt with `exit` or Ctrl-C, or use Unix
`detach` for an external that must outlive the session, never for a ral block.
See [[design/endings|endings]] for the timing-race rationale.

**Cancellation is hereditary.** A gather's birth hangs under the scope current
when the gather was entered — a worker's own node inside a worker, the run's
scope at a `gather` in a run — and a session birth under the session root. So
`cancel $h` reaches everything `$h` started, and an interrupt or a wall on a
run reaches its gathers' children but never a session worker. A worker
converts a `Cancelled` or `TimedOut` stop owned by any node but the session
root into its own `cancelled` / `timed out` error, so a
cancel made inside the session is a failure the awaiter can `try`, while a
stop the host owns passes as the escape it is. A gather that is not a worker
converts nothing.

See [[decisions/261020_gather|gather]] (I3–I5, I4′),
[[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]
(I6), [[internals/cancellation|cancellation]], and `docs/SPEC.md` §11.1,
§11.3.
