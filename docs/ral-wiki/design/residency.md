# residency: a session is a ledger of residents

**Everything that stays alive between runs — a detached worker, a sub-agent,
a schedule, a top-level binding — is a *resident*,
and a resident has exactly four facets: an identity in the session's
ownership tree, a typed capability that reaches it, a lease, and a probe.**
The session keeps one *ledger* of residents, in chapters that keep their own
representations, and every management surface — listing, the exit story,
cancellation cascade, `/clear`'s clear-epoch, `/resources` — is a fold over
that ledger, written once. Lifetime moves along one order of residency
grades, and the two halves of the system are its two traversal disciplines:
interactive work *discovers* its lifetime after the fact, agent work
*declares* it at birth. The full reasoning — the five registries that grew
independently, the wart this dissolves, why the deep fusion is refused — is
[[decisions/260705_session-ledger|session-ledger]].

## The four facets

- **Identity** — an id, with an owner edge into the session's tree (the
  agent tree, the shell that spawned the worker, the session that armed the
  schedule). Ownership is what cancellation cascades along.
- **Capability** — the typed value that reaches and controls the resident: a
  `Value::Handle` for a worker, an
  `AgentId` for an agent, a schedule id for a wakeup, a top-level *name* for
  a binding. Capabilities are deliberately not unified — they are the honest
  variance between chapters, never flattened into one value.
- **Lease** — a clock, an idle bound, a renewal signal, an optional
  backstop — including the degenerate case "none; legibility is the
  bound" (a `service`).
  leases-and-budgets is where the
  lease machinery itself lives; residency is the frame it sits inside.
- **Probe** — what the resident costs now, for the `/resources` fold.

Nothing session-lived escapes this: a live thing with no chapter is a review
defect, the resident-shaped completion of the probe convention
([[invariants/probe-convention|probe-convention]]).

## Resident vs. accumulator

The line between a resident and a mere *accumulator* (a viewport, the bus,
an inbox) is the capability: a resident can be reached and controlled by
id; an accumulator can only be measured and bounded. Both are probed — the
`/resources` fold spans both kinds — but only residents are listed,
cancelled, and leased. A viewport has no capability, and pretending it does
to buy uniformity would buy it at the price of a lie.

The accumulator's contents have a second characterisation, exact in scope:
**a viewport is always a fold's memo.** Resume seeds that memo from the session
record before the worker spawns; live `Signal::Fact`s step the same fold, while
`Signal::Transient`s touch only its provisional edge
([[internals/session-record|session-record]]). The viewport owns presentation
state around the memo — scroll, disclosure, the open line, the bounded window —
but no independent account of what happened. None of that grants it a
capability; the distinction says where state comes from, never who may reach it.

## The residency order

Residency states are graded by independence from the session — a *graded
partial order*, not a lattice: two residents can sit at the same grade with
incomparable capabilities, and no join is claimed. There is no *stopped*
grade: ral does not suspend, so a stopped child is resumed at once by
whoever waits on it rather than parked as a resident of its own
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).

| grade | state | population | entered by | left by |
|---|---|---|---|---|
| 0 | foreground | the run in progress | evaluation | completion, the wall, interrupt |
| 1 | background | detached handles (`spawn`) | birth | completion, `cancel`, lease reap, exit |
| 2 | durable | `service` workers | birth | completion, `cancel`, `/clear`, process exit |
| 3 | survives exit | `detach`ed processes | birth | outside the session's story |

Leases are the downward pressure — everything drifts toward reclamation
unless renewed.

## Two traversal disciplines, one order

- **Interactive work discovers its lifetime.** A human cannot know at
  launch that this build wants backgrounding; intent is revealed late, so a
  REPL user reaches for `spawn` once that becomes clear, or Ctrl-C and
  re-runs under `spawn` for work that was already in the foreground.
- **Agent work declares its lifetime.** A model states intent at birth —
  `spawn` is born at grade 1, `service` at grade 2 — and mid-flight movement
  (`promote`) stays deferred until a concrete need.

*Birth, not promote* is not a universal law; it is the declaration
discipline. Promotion is not a rejected mistake; it is the discovery
discipline's native gait. Both disciplines walk the same order — they
differ only in when a resident's grade is decided.

## The folds, written once

The chapters keep their own representations, locks, and homes
([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]):
the worker registry beside core's handles, the agent tree and schedules in
exarch. What is shared is the
small resident signature — identity, population, capability kind, lease
row, state label, cancel, defined once in
[[map/core/shell-state|`core/src/types/resident.rs`]] — and the folds
written against it:

- **list** — project a set of chapters into one table, marked by kind in a
  designator namespace of its own (`[wN]`) so different chapters can never
  collide.
- **warn** — the exit story: what survives, what is named. Shell exit
  names any worker handle still running first — it dies with the process,
  so naming it is its only farewell.
- **cascade** — cancellation follows ownership edges regardless of
  population. Cancelling a resident cancels what it owns: an agent's
  teardown reaches its own workers through its shell's durable root (a
  worker's cancel scope is a child of that root, so cancelling it walks the
  ancestor chain with no extra edge), and reaches its schedules and
  children through the tree itself (`Agent::parent`/`Agent::children`).
- **clear-epoch** — every session's inbox counts its own clears, bumped
  under the same lock the drain runs, and `/clear` bumps only that inbox's
  count. Anything settling across it is rejected at the inbox's own pop —
  whether by arriving through a `bus::Stamp` (an async agent's result, a
  worker's deferred surface batch, a scheduled wakeup) or by the stronger
  route of unconditional removal (a schedule, disarmed and dropped rather
  than tagged). The stamp is the addressed envelope `Mailbox::stamp` mints
  — destination and epoch captured as one value at composition — so what a
  worker carries is structurally the envelope of the inbox that will *read*
  it, and the rejection is addressed, not global: one tab's `/clear` leaves
  another tab's outstanding work alone.
- **probe** — the `/resources` projection, over residents and accumulators
  alike.

**Enumeration is not observation** is the ledger's own restatement of a law
each chapter already keeps: no fold renews a lease. Interest is naming a
resident through its capability, never scanning past it.

## The fusion refused

The interface is the unification; a single registry struct would be the
flattening. Fusing the chapters — one struct, one lock order, one capability
type — is rejected because the substrates genuinely differ: thread plus
result channel versus agent tree plus mailbox versus schedule plus wakeup,
each with its own ownership shape and its own probe. A prior draft of this
refusal also weighed fusing a pgid job chapter into the handle model — `fg`
becoming a handle's `await` — but ral does not suspend
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]), so there is
no pgid chapter left to fuse against; a handle's own eliminators (`await`,
`cancel`) are simply what they are, never analogues of a job verb that no
longer exists.

Not every chapter implements the resident signature, either, and that is
the same refusal at a smaller scale: exarch's agent tree, schedules,
and binding ledger each only ever hand out a bare snapshot type for
listing, built by cloning fields out from under a lock already dropped by
the time the snapshot reaches a caller. `cancel` takes `&self` alone, by
design, and none of those three snapshots carries a live handle back to its
own chapter — adding one would grow a listing snapshot into something that
either holds a lock past the call that built it or duplicates the
tree's own cancel methods. The ledger is precisely what makes refusing
these fusions affordable: unity lives in the signature and the folds, so
the capabilities — and the chapters that decline to unify further — can
stay typed and distinct without the system falling into pieces.

## See also

[[decisions/260705_session-ledger|session-ledger]] (the ADR this page
graduates from, and its full context/alternatives),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the handle model; the job-table separation this restates at the listing
layer), [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]
(the deferred survivor warning), [[decisions/260629_agent-binding-reaping|agent-binding-reaping]]
(the binding chapter), [[decisions/260617_scheduled-wakeups|scheduled-wakeups]]
(the schedules chapter), [[decisions/260619_terminal-lease|terminal-lease]]
(why `fg` is a host affordance), [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(why the chapters keep separate homes), [[invariants/probe-convention|probe-convention]]
(the probe facet as a checkable rule),
[[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]] (why there is
no stopped grade), [[map/exarch/agent|agent]],
[[map/core/shell-state|core/shell-state]].
