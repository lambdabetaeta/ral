---
status: active
---

# A signal contributes a cause to the join; it does not reach into a run frame

> The **spend** half is superseded by
> [[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]: "a cause
> raised for a settled run must not unwind the next one" is a claim about time,
> so the interrupt becomes a per-cause watermark of instants read against a
> frame's birth date, and nothing is reset. With no spend there is no spender —
> `REQUESTED_FOREGROUND`, `spend_foreground_request`, `faces_foreground`,
> `fronts_foreground`, the displaced-frame gate, and `overhearing` all go, and
> the aside simply shares the session's root. The join algebra, routing by
> minting, the private `fold`, and the deletions recorded below are untouched.

**Cancellation is a join-semilattice — `CancelCause` is a total order,
`cancel` is a `fetch_max`, and a scope's cancellation is the join of its own
element with every ancestor's. A signal handler holding no scope should
therefore *contribute an element*, not alias one. Two process-lifetime
`AtomicU8` cells carry the handler's contribution, and a scope folds a cell
into its join iff it was minted facing it.** Nothing is published, nothing is
restored, nothing dangles, and the routing question — which scopes see which
signal — is answered once, at mint.

## What the published slots got wrong

The slots held a raw `AtomicPtr` alias into a *run frame's* cancel flag,
republished per run and blind-save/restored by the run guard. Every
complication descended from that one shape:

- **A use-after-free reachable from a handler.** `request` loaded the slot at
  one instant and dereferenced it at another; a process-directed signal lands
  on an arbitrary unblocked thread, so those instants could straddle the
  frame's death ([[decisions/260726_cancel-slot-leak|cancel-slot-leak]]).
- **A LIFO requirement.** Blind save/restore reads back its intended
  predecessor only for publications that nest on one thread.
- **An unenforced one-publisher rule.** *At most one session per process
  publishes* was carried by a `bool` on `SessionState` that any code could
  set, and production violated it anyway (the REPL's hook shell publishes
  beside the main session).
- **A leak papering over the first.** One strong share of the scope's `Arc`
  per publishing run, for the life of the process.

The delivery failures were **measured**, not theorised. Letting every test
session publish fails the `ral-core` binary in roughly two runs in three:
`engine::wire_desk_tests`' parked enquiry reads another test's cancelled run
and raises a spurious `cancelled`/130, and
`engine::engine_session_tests::cancel_settles_an_in_flight_run_promptly` waits
out its full 20 s ceiling because the request landed on a foreign scope.

## The algebra

    cancelled(s) = ⨆ over chain(s) of ( flag(n) ⊔ request_cells(n.hears) )

- **`REQUESTED_FOREGROUND`** carries the user's interrupt of the foreground —
  Ctrl-C, Esc, a front-end's `Control::Cancel`.
- **`REQUESTED_ROOT`** carries the process-wide shutdown request — SIGTERM,
  SIGHUP, Ctrl-`\`.
- `ScopeNode` gains one **immutable** `hears: u8` (bit 1 folds the
  foreground cell, bit 2 the root cell), fixed at mint. There is nothing to
  install and nothing to restore, so no LIFO requirement and no lifetime
  coupling between a scope and a `static`.
- `request_foreground_cancel` / `request_root_cancel` keep their names and
  signatures; each body is one safe `fetch_max`. **Every `unsafe` in
  `cancel.rs` is gone**, along with the load-then-deref and the null check.
- **One fold, one place.** A single private `CancelScope::fold`;
  `is_cancelled` and `cause` are its only callers. `ScopeNode`, its flag, the
  hears bits and both cells are private to `core/src/process/cancel.rs`, so
  no future observer can read a cancellation except as the whole join. That
  structural enforcement is what the change is bought for.

## Routing by minting, not by a session flag

`spawn_thread` builds a worker's shell from a fresh default `SessionState`, so
a session boolean was never the right discriminator. Which signals reach a
scope is now decided by which constructor made it:

| minted by | folds | who mints it |
|---|---|---|
| `CancelScope::root` | — | tests, a forked session |
| `DurableRoot::new` | — | every `Shell::new` |
| `DurableRoot::signal_facing` | root | `Shell::face_signals` |
| `DurableRoot::overhearing` | root **and** foreground | `Shell::overhear_signals` |
| `DurableRoot::foreground` | whichever of the foreground cell its root does not already fold | the run doors |
| `DurableRoot::worker` | — (its root's bits, through its parent) | `spawn_thread`, the boot frame |
| `ForegroundScope::child` | the parent's bits | nested scopes, deadline windows |

- `Shell::new` mints **non-facing**; each primary host boot calls one new
  `Shell::face_signals` — the REPL session, batch entry, `engine_session`,
  exarch's trunk seat. `fork_session` re-mints a non-facing root by
  construction, so a sub-agent is deaf to the requested causes and its host
  stops it through `Shell::cancel_handle`.
- A detached worker folds `REQUESTED_ROOT` **through its parent**, so a SIGTERM
  reaches it, while `REQUESTED_FOREGROUND` never appears on its chain: it cannot
  absorb a foreground interrupt, by the shape of its fold rather than by a
  rule someone must remember.
- Several facing sessions in one process are well-defined — they *share* the
  cause — so nothing has to police uniqueness.

## Hearing is not facing

A scope **hears** a cell when anything on its chain folds it, and **faces** one
when *that node* was minted against it. Facing is the narrower relation, and
the only one the spend reads (below).

The distinction earns its keep on the REPL's **hook shell** — a second `Shell`
(`Shell::child_from`, `ral/src/repl/plugin.rs`) that evaluates arbitrary plugin
code while the session sits at its prompt. It must be interruptible; it must
never retire an interrupt. Making it *face* would give it both — its run's
frame displaces its own boot frame, so its entry reads as top-level and spends
— which is the missed-cancellation class this ADR just closed, re-entered by
another road. So an **aside** is minted differently: `overhearing` folds *both*
cells **on the root**, and `foreground` then adds only the folds its root lacks
— nothing. Every frame of an aside hears the interrupt for its whole life;
none faces it; none can be mistaken for a top-level command.

The bit that changed is *where on the chain the fold sits*, not a new flag: an
aside's fold is its root's and permanent, the session's is its run frame's and
per-entry. Only something minted per entry can be spent per entry.

One consequence, stated plainly: a worker started *inside* an aside hears the
interrupt too, since it hangs off the aside's root. Work begun inside an aside
is as interruptible as the aside — the detached-worker exemption belongs to the
session, whose root folds only `REQUESTED_ROOT`.

## Spending, and what is never spent

- **`REQUESTED_ROOT` is never reset.** Its causes are process-fatal and one-way.
  A bonus falls out: an idle SIGTERM now latches and is caught at the REPL's
  prompt-boundary root check, closing the residual
  [[decisions/260706_signals-are-causes|signals-are-causes]] deferred.
- **`REQUESTED_FOREGROUND` is spent by exactly one kind of entry: a top-level
  command of the session that owns the process's foreground.** In
  `RunGuard::install`, both halves read the frames' minting and neither asks a
  question about the `Shell`: the entering frame must *face* the cell (a deaf
  session's does not, and neither does an aside's), and the displaced frame
  must not (it would then be an outer run in flight whose contribution the
  entry must not destroy). Not at the compile door — a hook run bypasses that
  entirely.
- `process::clear()` stays escalation-ladder-only.

## The semantic change this accepts

Joining a shared element is *sharing*, not shadowing: **a Ctrl-C during a
nested run now unwinds the whole nest when the outer run resumes**, where the
shadowing slot left the outer scope untouched. This is what a POSIX shell
does, and it is the honest reading of the algebra. The only in-tree
re-entrant `Shell::run` sites are inside `#[cfg(test)]`, so no production path
changes behaviour today.

## The parked enquiry reads a scope, not a global

`EnquiryDesk::enquire` gains a `&CancelScope` parameter; `Shell::enquire`
passes the run's own `run.cancel`. The wire desk's park polls `cause()`
instead of a process-global read, which also closes a latent gap: the park now
sees reaper deadlines and cancelled session handles, which a request-cell-only
read missed.

## Deleted

`CancelSlot` and its `Drop`; `publish` / `publish_foreground` /
`publish_durable_root`; the dereferencing `request`; `flag_ptr`; both
`AtomicPtr` statics; every `unsafe` in `cancel.rs`; `foreground_cancel_cause`;
`SessionState::publishes_signal_slots` (field, init default, fork flip,
`RunGuard` gating); the `RunGuard` slot fields and their drop-order argument;
and the leak with its justification. A single poison-tolerant test lock
survives, for the tests that touch the cells or the spend.

## Still open

exarch's per-exchange `Token` slot (`exarch/src/agent/cancel.rs`) is the same
alias pattern — a published pointer into a per-exchange flag, kept safe by the
same leak — and admits the same cure: an request cell the token folds.

## See also

- [[internals/cancellation|cancellation]] — the updated map of the whole
  mechanism.
- [[decisions/260726_cancel-slot-leak|cancel-slot-leak]] — the interim leak
  this retires, and the measurements it recorded.
- [[decisions/260706_signals-are-causes|signals-are-causes]] — the collapse of
  signal delivery onto the cause lattice, whose substrate this replaces.
- [[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]] — the
  one-publishing-session rule, now dissolved into how a scope is minted.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] — the
  root/foreground split the newtypes carry.
- [[map/core/io-process|io-process]], [[map/core/shell-state|shell-state]].
