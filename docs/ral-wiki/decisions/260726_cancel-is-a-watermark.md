---
status: active
---

# Spend was a birth date implemented destructively: the interrupt is a watermark

**A cause raised for a run that has settled must not unwind the next one — a
statement about *time*, which the resettable cell made by zeroing shared state.
Zeroing needs a unique zeroer, and the whole authority apparatus followed from
that. Re-index by time and the mechanism is monotone again: the interrupt
becomes a per-cause watermark of instants, a foreground frame records its birth
instant at mint, and it observes exactly the causes stamped after it. Nothing
is ever handed back, so nobody needs the right to hand it back.**

## The two ambient causes are different kinds of proposition

[[decisions/260726_cancel-is-a-join|cancel-is-a-join]] gave both the same shape
— an `AtomicU8` holding a `CancelCause` — and then had to reset one of them.
The asymmetry is now *typed* rather than procedural:

- **Shutdown is absolute.** `REQUESTED_ROOT` is a plain lattice element: once
  raised it holds for every observer forever. Unchanged — it was always
  correctly shaped.
- **An interrupt is temporal.** It is aimed at whatever was running when the
  key was struck, so it is true only *relative to a birth instant*: a
  Kripke/presheaf reading rather than a global truth value. `CLOCK: AtomicU64`
  ticks once per raised interrupt, `STAMPED[c]` records the instant cause `c`
  was last raised at, and a frame born at `b` observes `⨆{ c : STAMPED[c] > b }`.

The join is unchanged in shape,

    cancelled(s) = ⨆ over chain(s) of ( flag(n) ⊔ ambient(n.hears) )

with `Hears` a three-way sum — `Nothing`, `Shutdown`, `InterruptsSince(u64)` —
rather than a bitset, because the third case carries a payload the others do
not. The prior design tried to make a time-indexed proposition absolute and
then patched the damage with a reset.

**The handler stays async-signal-safe:** a `fetch_add` on the clock and a
`fetch_max` on one stamp — two lock-free read-modify-writes, no allocation, no
lock. `fetch_max` still buffers the join, so repeated Ctrl-C re-stamps without
downgrading a stronger cause. The escalation ladder is untouched.

### Why one stamp per cause, not one packed word

A frame reads a *suffix* of the escalation order, and a single word can keep
only one of the two coordinates faithfully:

- packed **instant-major**, a young weak cause erases an old strong one — a
  Ctrl-C after a deadline would lose the `timed_out` classification
  `run_built` reads back from `foreground.cause()`;
- packed **cause-major**, an old strong cause hides a young weak one from
  every frame born between them.

Five `AtomicU64`s, and up to five extra loads at a stamped node — human-
frequency events. Pinned by `each_cause_keeps_its_own_instant`.

## The repair it forces, and the hole that repair closes

The run door minted every foreground **from the root**, so a nested run's frame
was a *sibling* of the frame it nested in. Sharing-not-shadowing held only
because both siblings folded the same cell; under watermark semantics a sibling
born after the interrupt would miss it.

**The run door now mints each entry as a child of the frame it displaces.**
`RunGuard::install` already saved and restored that frame LIFO, so the cancel
tree finally *is* the dynamic extent it modelled.

- A nested run observes the interrupt its outer run carries, by ancestry.
- **A latent hole closes:** an outer run's wall elapsing now reaches a nested
  run in flight. It never did — the siblings shared no node — though
  "cancellation is the join with every ancestor" always claimed it.
- The run door keeps **no conditional logic**: `dispatch` mints
  `durable_root().foreground(&self.run.cancel)` and `RunGuard::install` is a
  swap.

## The aside needs no constructor of its own

The REPL's hook shell shares the session's `DurableRoot`
(`Shell::join_session`) rather than minting one, which also closes a
reachability hole: a sibling root never folds the session root's own flag, so a
`Shell::cancel_handle` cancel — how exarch stops a session — would have killed
the session and left its aside running. Pinned by
`cancelling_a_session_by_handle_reaches_its_aside`.

Interruptibility then follows from the watermark with nothing added:

- A Ctrl-C struck **during** a hook is younger than the hook's frame, so the
  hook unwinds; arbitrary plugin code stays interruptible for its whole life.
- A Ctrl-C aimed at a command the session was **already** running is older than
  every frame the aside will ever mint, so the aside can neither absorb it nor
  keep it from the run it was aimed at — not because it lacks a right, but
  because there is no retirement operation in the system. Hear-without-spend
  stops being a category that needs a name.

**One behaviour changes:** an aside's run no longer observes an interrupt
raised before it began. The arrangement does not arise in production — the REPL
prepares its hook shell at the prompt, never beside a command in flight — and
the property the spend existed to protect is strictly stronger now.

## Concept count

| | before | after |
|---|---|---|
| root kinds | deaf / facing / overhearing | deaf / facing |
| extent mechanisms | ancestry, plus a shared cell for siblings | ancestry, now matching the dynamic extent |
| authority concepts | `fronts_foreground`, `faces_foreground`, the displaced-frame gate | — |
| destructive operations | `spend_foreground_request` | — |

The rule, in one sentence: *a frame observes every cause recorded on its chain,
plus every ambient interrupt younger than a frame on it.*

## Deleted

`REQUESTED_FOREGROUND` and `spend_foreground_request`;
`ForegroundScope::faces_foreground`; `SessionState::fronts_foreground`;
`DurableRoot::overhearing` and `Shell::overhear_signals`; the `FOLDS_*` bitset;
the gate in `RunGuard::install`. `DurableRoot::foreground` gains the frame it
nests under. `CancelScope::child` folds nothing of its own — the parent is on
its chain, so copying its `hears` was always redundant.

Everything else [[decisions/260726_cancel-is-a-join|cancel-is-a-join]] bought
stands: the join algebra, routing by minting, the single private `fold`, the
absent `unsafe`, the typed root/foreground relation, the deaf fork, the spared
detached worker, and the scope-reading enquiry park.

## See also

- [[internals/cancellation|cancellation]] — the whole mechanism as it now runs.
- [[decisions/260726_cancel-is-a-join|cancel-is-a-join]] — the join algebra this
  keeps and the spend half it supersedes.
- [[decisions/260706_signals-are-causes|signals-are-causes]] — the collapse of
  signal delivery onto the cause lattice.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] — the
  root/foreground split the newtypes carry.
- [[map/core/io-process|io-process]], [[map/core/shell-state|shell-state]],
  [[map/repl/plugins|plugins]].
