---
status: active
generated_at_commit: fe9cd32d
verified_at_commit: 8826b170
anchors: [await_handle, join, claim_first, claim_settled, claim_and_stop, Settler, Cradle, Brood, Birth, convert_own_stop, TitlePolicy, Stamp, Owner, WorkerRow, builtin_worker, spawn_form]
---

# `await` is the block run here, and a worker ends with its children

> Amended 2026-10-20 by [[decisions/261020_gather|gather]]: there is one
> ownership mechanism, the brood, and `gather { … }` is its syntax; a worker's
> body is an implicit gather.
> - **I2** names two data observations: `poll` of one handle, and
>   `first-settled`, the index of the first of several to settle; neither
>   writes or claims, and both treat a claimed handle as `await` does.
> - **I3–I5** name a *gather* as the third owner in place of the calling
>   expression: a `gather { … }` or a worker's body owns every `spawn` and
>   `watch` born inside it, hung under the scope current when it was entered;
>   the session owns a birth with no brood in scope; nobody owns `detach`. I4
>   is a gather's exit rule, a worker's base applying it to its own brood, and
>   I5's call-owned node is the gather's entry scope.
> - **I4′ is added**: a gather's exit waits, within the grace, for every child
>   it started whose scope is cancelled and whose thread has not ended,
>   however it was cancelled.
> - **`par`/`all` as a `Litter` is retired**: `par` is prelude code over
>   `gather`, `spawn`, `first-settled`, `poll` and `await`, keeping input order
>   in bytes and values; the `all` form is deleted (`both` covers two blocks);
>   `race` is kept in Rust over handles already held, since it must drain
>   losers no gather owns; `first-of` races fresh blocks. The "Three owners"
>   table's calling-expression row and the account of `par` and `all` below
>   are superseded.
> - **`Birth::Call` is subsumed**: every gather-owned birth, a worker's child
>   included, is registered, uses the run's live surface and announces no
>   birth and no `` `done ``; only a session birth is announced. `workers`
>   shows `` `gather `` where it showed `` `call ``.
> - **The rejection of "a `gather { … }` delimiter" below is reversed.** One
>   ownership mechanism beats two: a gather is the worker's own scope given
>   syntax, not a fourth owner, and it replaces the call owner rather than
>   joining it; and the argument that it was redundant with `par`, `all` and
>   `race` no longer holds once those are prelude code over `gather`.

> **Amended 2026-10-20.** A handle's result is claimed once. `await`, `race`
> (its winner and every loser), `cancel` and the handle's owner each *claim* it.
> A handle is one lock over `HandleState` — `Running`, `Settled`, `Claimed` —
> and a claim is one transition on it, so of two claimers exactly one
> succeeds, and the claimer moves the result out of the handle instead of
> copying it from a cache every `await` could read. A second `await` of a
> claimed handle fails with `<verb>: this worker's result was already
> claimed`, an ordinary error, `race` skips a claimed handle and fails so only
> when every handle it was given is claimed, and a second `cancel` does
> nothing. `poll` is a pure
> observation: it claims nothing on either arm, so a settled poll leaves the
> handle to be awaited and its entry in the registry, and it fails as `await`
> does on a claimed handle, as `is-done` then does. A worker's join claims
> only the children it wins, so an outside `await !{worker <id>}` and the join
> never both replay a child. The check is at run time; types are unchanged.

**`await $h` behaves as if `h`'s block had run at the `await`: the bytes it
wrote are written to the awaiter's own streams, and it ends as the block
ended. Every concurrent block has exactly one owner, fixed at birth — the
calling expression, the session, or nobody — and a worker is a scope over the
workers born inside it: it ends only after they have, and cancelling it
cancels them.** `spawn [TITLE] BODY` is a control operator whose title is
optional in the syntax and required where a host's policy says so; `par`,
`all` and `race` are structured fan-out that leaves nothing behind; `service`,
the worker leases and `defer` are gone; `workers` lists the registry as data,
and `worker <id>` is the one door back to a handle.

## Decision

Ten invariants, each named so a test and a page can cite it.

- **I1, exact `await`.** For a handle `h` born by `spawn [T] B` where `B`
  would have run with the same environment, `await $h` is observationally
  `!$B` run at the `await`: the same value or the same raise, and the bytes
  `B` wrote to its stdout and its stderr written, in that order, to the
  awaiter's current stdout and stderr sinks. The `await` claims the handle's
  result, which is claimed once: a second `await` fails and replays nothing
  ([[invariants/await-is-the-block-run-here|await-is-the-block-run-here]]).
- **I2, one data observation.** `poll` is the only data-valued observation,
  and it is total on an unclaimed handle: `` `pending [stdout, stderr,
  elapsed] `` or `` `settled [stdout, stderr, elapsed, outcome] ``. It never
  writes, never claims, and never raises for an ordinary failure; a block
  stopped by its scope settles as `` `err ``. An `exit`, a foreign stop and a
  panic re-raise through it, and a handle `cancel` or `await` claimed fails
  it.
- **I3, three owners.** Every concurrent block has exactly one owner, fixed
  at birth: the *calling expression* (`par`, `all`, `race`: ended before the
  call returns), the *session* (`spawn`, `watch`: ended by completion,
  `cancel`, or the session's end), or *nobody* (`detach`). There is no fourth
  owner and no lease.
- **I4, a worker ends with its children.** A worker's body ends, then each
  child still unclaimed is claimed and awaited in birth order with I1's
  semantics, a child another claimer won being skipped; the first
  failure among them is the worker's, and the rest are cancelled and drained
  within the cleanup grace. A body that halts cancels and drains them too
  ([[invariants/a-worker-ends-with-its-children|a-worker-ends-with-its-children]]).
- **I5, hereditary cancellation.** A loose birth's node is a child of its
  parent worker's node inside a worker, of the session root in a run; a
  call-owned child's node is a child of the calling expression's current
  scope. `cancel $h` reaches everything `$h` started.
- **I6, a cancel inside the session is the worker's own failure.** A worker's
  base converts a `Cancelled` or `TimedOut` stop whose owner is any node but
  the session root into the handle's `worker was cancelled` /
  `worker timed out` error. A stop the host owns passes as the escape it is.
- **I7, identity is the id; the title is for people.** Every worker has a
  `WorkerId` and a title. `worker <id>` reacquires any live or retained
  worker's handle in every host; nothing looks a worker up by title.
- **I8, a listing is data.** `workers` returns records of ids, titles,
  owners, states and times, never a handle, so it crosses the engine
  protocol. A uniform list of records renders as aligned rows by the pretty
  printer's own rule, shared by every host.
- **I9, a worker knows its own clock.** Every handle carries a birth `Stamp`
  (one instant on the monotonic and the wall clock) and, once its thread
  ends, an end `Stamp`. Everything that reports time reads those two cells.
- **I10, the notice is truthful and actionable.** The completion notice
  reports the worker's real ending, its title, how long it ran, and the one
  call that gets its result back: `await !{worker N}`.

The surface:

```text
spawn [TITLE] BODY : control operator.  TITLE : String;  BODY : U(F^ε α)
                     the form : F (Handle^ε α)
                     title optional in the syntax; a host may require it
all [k: BODY_k, …] : control operator; each BODY_k demanded as a value, U(F α_k)
                     the form : F [k: α_k, …]               call-owned
await   : ∀ε α. Handle^ε α → F^ε α                          exact (I1)
race    : ∀ε α. [Handle^ε α] → F^ε α                        the winner exact; losers cancelled and drained
poll    : ∀ε α. Handle^ε α → F <pending: [stdout: Bytes, stderr: Bytes, elapsed: Float]
                               | settled: [stdout: Bytes, stderr: Bytes, elapsed: Float,
                                           outcome: <ok: α | err: ErrorRecord>]>
cancel  : ∀ε α. Handle^ε α → F Unit
par     : ∀α β. U(α → F β) → [α] → Int → F [β]              call-owned
watch   : ∀ε α. String → U(F^ε α) → F (Handle^ε α)          ral hosts only
worker  : ∀ε α. Int → F (Handle^ε α)                        a boundary, every host
workers : F [[id: Int, title: String, owner: <session | worker: Int | call>,
             state: <running | done: <ok | err: ErrorRecord | exited: Int
                                       | stopped: CancelCause | panic: String>>,
             started: Int, elapsed: Float, settled: <some: Int | none>]]
```

`Handle^ε α` prints `Handle (Returns α)`, `Handle Command` or `Handle (ν α)`:
a handle is a suspended producer, and its print says which. The one pattern
the guidance teaches is `let h = spawn "building" { cargo build }`, other
work or the end of the turn, then `await $h` — or `await !{worker 7}` from the
notice.

## The equation and its residue

`await $h`:

1. waits, cancel-aware, for `$h` to settle, then claims its result; the wait
   polls the awaiter's scope, so an interrupt or a wall unwinds the wait,
   claims nothing, and leaves the worker to be awaited later;
2. writes the worker's stdout bytes to the awaiter's current stdout sink,
   then its stderr bytes to the current stderr sink — inside a capture,
   `let x = await $h` captures them; inside a worker they land in that
   worker's buffers; under `2> f` the stderr goes to `f`;
3. ends as the block ended: a value is returned (admitted against the site
   when the handle came through `worker <id>`), an error is raised as it
   stands, span and command included, an `exit` exits the awaiting run, a
   stop is re-raised, a panic resumed.

The claim moves the result out of the handle: an `await` of a handle already
claimed, by an earlier `await`, by `race` or by `cancel`, writes nothing and
fails with `await: this worker's result was already claimed`. A worker stopped
from elsewhere, by its parent's cancel or a wall, ends with its own
`worker was cancelled` (status 143, `` `cancelled ``), which the `await` that
claims it raises. A cancelled parent's halt claims its children, so an outside
`await` of one meets that ending only if it claimed the child first.

**The handle carries the block's grade.** `spawn [T] B : F (Handle^ε α)` for
`B : U(F^ε α)`, and `await : Handle^ε α → F^ε α`, so `await` of
`spawn { echo hi }` is a `Command`, `let x = await $h` binds `"hi"` exactly as
`let x = { echo hi }` would, and `await` of `spawn { echo pre; return 5 }` is
`Returns Int` that writes `pre`, as the block inline. No grade is new: under
[[decisions/260930_graded-f|graded-f]] a grade is a producer kind, not an
effect, so replaying bytes changes nothing about `ε`.

**The equation holds** for the value or raise and for the bytes on the two
standard streams, the effects of `B` having happened earlier, concurrently,
in the worker's own shell. It does not hold for:

- *shell state*: `cd`, `within [env:]` and the status set inside `B` never
  flow back;
- *the interleaving of the two streams*: the buffers are replayed one after
  the other, so a block alternating `echo` and `warn` replays all of its
  stdout, then all of its stderr;
- *a block that spawns*: run inline in a run, its inner `spawn` would be a
  loose session worker; run as a worker, I4 joins the child before the outer
  handle settles, so the awaiter sees the child's bytes and failure too;
- *a cancelled or timed-out block*, whose ending the inline run could never
  have;
- *`exit`*, which ends the run that awaits — possibly a later turn.

## Three owners

| owner | born by | node under | registered | birth announced | output | ended |
|---|---|---|---|---|---|---|
| calling expression | `par`, `all` (`Birth::Call`); `race` adopts the handles it is given | the call's current scope | yes: the cap counts it, `workers` shows `` `call `` | no | the call's live sink, as a pipeline stage's; no deferred flush, no `` `done `` | claimed by the call before it returns; on any failure cancelled and drained within the grace |
| session | `spawn`, `watch` | the enclosing worker's node, else the session root | yes | yes | the deferred sink, flushed once with a `` `done `` | completion, `cancel`, the session's end |
| nobody | `detach` | — | no | as before | none | never, by ral |

A forgotten session worker is bounded by the session and by nothing else: the
admission cap refuses a birth past it (64 under exarch) with a message naming
`await`, `cancel` and `workers`; settled results are retained for a bounded
number of ral calls; `Roster::drop` cancels and drains everything at the
session's end.

**A call-owned child is a stage of the call, not an errand of the session.**
`par` and `all` share a `Litter` of the children the call owns in birth order;
each child is claimed with `await_handle`, the exact `await`, and `Litter::end`
is `end_unclaimed`, which claims every child not yet claimed, stops it, and
waits for its thread up to the grace. `race` claims its winner and runs
`end_unclaimed` over every handle on every exit path.

- **`par f items jobs`** keeps at most `jobs` children running (`jobs <= 0`:
  one per item), claiming the oldest whenever the window is full and the rest
  in order at the end. Any failure, `exit`, stop or panic ends the litter
  before it leaves. The result is in input order, and each child's bytes
  replay as it is claimed, so item 1's output precedes item 2's whatever order
  they finished in.
- **`all [k: B_k, …]`** births each task, titled by its label, then claims
  each in declaration order into `[k: v_k]`; the same failure path. Each block
  is demanded as a value, as a function argument is, so a command's value is
  its output.
- **`race hs`** waits for the first to settle, claims it exactly, then claims,
  stops and drains every loser. A handle another caller claimed is skipped;
  with every handle claimed, at entry or mid-wait, `race` fails as `await`
  does. An empty list is `race`'s own error. It
  starts nothing, so "owned by the call" means that when `race` returns, every
  handle it was given has ended or is cancelled and draining.

The grace is cooperative: a child that ignores cancellation outlives its call
by at most the grace, claimed and out of the registry, as a `guard` cleanup
and the session wipe already accept.

## A worker's two rules, and why it waits

A worker's shell carries a *cradle*: where its loose births hang (its own
node) and the *brood* that answers for them (their handles, in birth order).
A run's cradle hangs at the session root and answers for nothing; a pipeline
stage shares its parent's, so a `spawn` in a stage inside a worker belongs to
that worker. The worker's base applies, after the body:

- **the return rule** (the body returned `v`): claim and await each child no
  one has claimed, in birth order, with I1's semantics, skipping one another
  claimer wins first — its bytes land in the parent's buffers,
  its failure becomes the parent's, as if the parent had written the `await`
  on its last line. The first failure stops the rest;
- **the halt rule** (the body raised or escaped, a child failed under the
  return rule, or a stop cut its wait): claim every child not yet claimed, stop
  it, and wait for its thread up to the cleanup grace *without polling*,
  since the parent's own scope may be what was cancelled; then propagate the
  halt;
- **the panic path**: claim and stop every child not yet claimed; no drain.

**The normal return waits rather than cancels.** A model that writes
`spawn "all" { spawn "a" {…}; spawn "b" {…} }` has written something natural;
under a cancel rule it would silently lose both children, while under the
wait rule the parent's result is the children's, nothing is lost, and a
child's failure reaches the one place that reports once, the parent's
`` `done ``. It is also what makes I4 compose with I1: `await` of the parent
equals the parent's block run here, its last implicit step included. The wait
is bounded by nothing but the children, exactly as their own bodies are:
`spawn "a" { spawn "b" { sleep 100 }; 1 }` settles after 100 s. A parent that
wants a child to outlive it should not have been the one to start it.

A run has no brood: its loose births hang at the session root, and there is
no return rule for runs.

## Stops at a base

With I5, `cancel $parent` strikes the parent's node, and a grandchild would
otherwise meet a stop naming a node not its own — a foreign stop that ends an
agent's turn as `Ending::Stopped` for a `cancel` the agent made itself. So a
worker's base converts iff the stop is a `WorkerStop` (`Cancelled`,
`TimedOut`) **and its owner is not the session root** (`convert_own_stop`).
Root causes (`Terminated`, `Aborted`) and `Interrupted` are no `WorkerStop`
and pass unchanged, as does a stop that names no node. A wall's `TimedOut` on
a run node reaching a call-owned child converts, harmlessly: the combinator's
own next poll raises the real stop, and its halt path cancels the rest. This
amends law 9 of
[[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]]:
a worker consumes every in-session `WorkerStop`, not only one naming its own
node.

## Why `spawn` is syntax

[[invariants/fixed-arity|Fixed arity]] and
[[invariants/optionality-via-variants|optionality-via-variants]] make it a
hard rule that no value's signature spells an argument the call site may leave
out ([[decisions/260812_no-value-has-an-optional-argument|no-value-has-an-optional-argument]]).
The one place an optional part already lives is syntax: `if` without `else`.
So the title can be optional only if `spawn` is a control operator, parsed as
`spawn BODY` or `spawn TITLE BODY` — one operand, and a second if another
follows before the command's end or a redirect. The principle it installs:
**optionality lives in syntax or in data, never in a value's arity**; `if`'s
`else` and `spawn`'s title are its two syntactic cases. The change is honest
on its own terms too: every other form that runs a body under a frame of its
own is a control operator, and `spawn` was the exception. `spawn` and `all`
join the reserved words, so neither can be a binding or a command name.

**The title policy is the run's.** `Run` and `Mooring` carry
`worker_title: TitlePolicy` (`Optional | Required`); the ral hosts leave it
`Optional`, exarch sets `Required`, and an untitled `spawn` under `Required`
is refused at the step, before any birth: `spawn: this host requires a title:
spawn "what it does" { … }`. It sits beside `worker_cap` and `cleanup_grace`
for the reason `detach`'s description is validated at the door: legibility is
a host policy, and one door validates it (`legible_title`: non-empty, one
line). An untitled worker is titled `block at <site>`.

The title is the handle's `title`, the birth record's, the `` `done ``
event's, the notice's parenthesis and `workers`' column. `watch` stays a
builtin with its mandatory label: both its arguments are mandatory, so it
needs no syntax, and a host must still be able to *lack* it
([[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]).

## Why `service` and the leases went

With no lease to be exempt from and every worker titled, `service` is a titled
`spawn`, and its durable class goes with it: the registry has owners, not
classes. The leases go outright: the idle-observation lease reaped any job
left unobserved for an hour, which is exactly what the guidance told the
model to do with long work. `ReapCause::Retention` stays, as the one policy reap, and it removes
only settled results nobody claimed (never one a running parent will still
join).

What the leases kept that I9 needs became the handle's clock: `Stamp { mono,
wall }`, a birth stamp set at `HandleInner::new` and an end stamp set by the
worker's `Settler` on every exit path of its thread, from which `elapsed` (a
running worker's age, a settled one's run time) and `settled_at` are derived,
each in one place. `poll`, `workers`, the `` `done `` event and the notice
read them and nothing else.

## Listing

A listing that returned the registry as a language value holding live
`Value::Handle`s could never cross the host seam, since a handle is not
ground; that is why the earlier `workers` was retired. The constraint stands
and is designed around: `workers` returns data only — one row type,
`WorkerRow`, shared with the `` `workers `` probe (which adds the host's
`retention-left`) — and `worker <id>` (`builtin_worker`, a boundary) is the
one door back to a handle, composed in the same call: `await !{worker 7}`.
State is read without claiming. An unknown id fails naming `workers`. The
printer renders a non-empty list of records with one label sequence as one row
per element, padded per column, still surface syntax, so no worker-specific
code renders anything.

## Alternatives rejected

- **A total `await` returning a settle record** (`{value, stdout, stderr}`).
  It made the common shape safe by making it lie: a command's output did not
  appear where the block was awaited, `let x = await $h` bound a record no
  inline run would, and `defer`, the total idiom built on it, woke the model
  with `exit 0` for a failed body. The agent's turn is the exception
  boundary; `try { await !{worker 7} }` is one line.
- **A `gather { … }` delimiter** joining the spawns in its body. A fourth
  owner beside the three, and redundant: a worker already is the scope its
  births join (I4), and fan-out that must leave nothing behind is a call to
  `par`, `all` or `race`.
- **A checker-level title policy**, a `HostSurface` flag threaded into the
  `Signature`: four more seams for one bit, against one runtime door.
- **Lookup by title** (`worker "building"`). Titles are not unique, and a
  listing that shows ids makes it redundant; the ambiguous case is thereby
  never a case.
- **A cancel-on-return rule** for a worker's unclaimed children. It loses the
  natural `spawn "all" { spawn "a" {…}; spawn "b" {…} }` silently and breaks
  I1 for any block that spawns.
- **Re-clocking the idle lease**: a longer bound, or renewal by more kinds of
  observation. Any lease reaps a job that runs as long as it was told to;
  what bounds a forgotten worker is its owner, and for a session worker that
  is the session.

## Consequences

- Amends [[decisions/260615_poll-total-failed-arm|handle-settle]] (`await` and
  `race` no longer unwrap a settle),
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
  (parenting per the cradle; `par`, `all`, `race` structured; the orphan
  window closed),
  [[decisions/260617_long-running-work|long-running-work]],
  [[decisions/260705_session-ledger|session-ledger]] (the durable grade gone),
  [[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]
  (`service` deleted; `detach` stands),
  [[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]]
  (`worker` replaces `service-handle`),
  [[decisions/260930_graded-f|graded-f]] (`Handle` carries the grade), and
  [[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]]
  (law 9's owner rule; "`par` stays prelude code" retired, and with it that
  page's rejection of `par` in Rust: the nursery it would have rebuilt is the
  call-owned birth path, shared by `all` and `race`).
- `await` raises by default again, in a host that pays a turn for a raise.
  Decided: the notice says the truth, and `poll`'s settled arm is the one way
  to read a failed worker's output without `try`.
- The claim moves a worker's bytes out of its handle, so a 16 MiB buffer
  written into a capture is held twice only while it is written; the
  capture's cap applies as to any command.
- `spawn` and `all` are reserved: a script that bound either fails to parse,
  naming the keyword.
- The row rule reaches every uniform record list a host shows. Intended.
- Left out: lookup by title; any change to `watch`, `detach`, the cap,
  retention, the binding lease, `Roster::drop`, `audit`, `commands`,
  `succeeded`, `bytes-to-string`, `is-done`; a return rule for runs; any
  renderer outside the pretty printer; any change to a pipeline stage's parent
  beyond sharing the cradle.

## Where

`await_handle`, `join`, `builtin_worker` in
`core/src/builtins/concurrency/eliminate.rs`; `HandleState`, `Sample`,
`Settler` in `core/src/types/handle.rs`; `Birth`, `convert_own_stop` in
`core/src/builtins/concurrency/birth.rs`; `spawn_form` in
`core/src/builtins/concurrency.rs`; `Cradle`, `Brood` in
`core/src/types/handle/brood.rs`; `Stamp` in `core/src/types/handle.rs`;
`Owner` in `core/src/types/shell/workers.rs`; `TitlePolicy` in
`core/src/types/mooring.rs`; `WorkerRow` in `core/src/protocol/probe.rs`. The
runtime narrative is [[internals/cancellation|cancellation]] and
[[internals/output-capture-and-detachment|output-capture-and-detachment]];
the normative text is `docs/SPEC.md` §11.
