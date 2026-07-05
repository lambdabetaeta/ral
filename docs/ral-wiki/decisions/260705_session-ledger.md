---
status: proposed
---

# A session is a ledger of residents

**Everything that stays alive between turns — a detached worker, a stopped
pipeline group, a sub-agent, a schedule, a top-level binding — is a
*resident*, and a resident has exactly four facets: an identity in the
session's ownership tree, a typed capability that reaches it, a lease, and a
probe. The session keeps one *ledger* of residents, in chapters that keep
their own representations, and every management surface — listing, the exit
story, cancellation cascade, `/clear` generations, `/resources` — is a fold
over that ledger, written once. Lifetime moves on one order of residency
grades, and the two halves of the system are its two traversal disciplines:
interactive work *discovers* its lifetime after the fact (Ctrl-Z, `bg`, `fg`,
`disown`), agent work *declares* it at birth (`spawn`, `service`). One theory,
several instances; the unification is the signature, never a flattening of the
structs.**

## Context

### Five registries grew independently

- **The worker registry**
  ([[decisions/260705_leases-and-budgets|leases-and-budgets]]): every detached
  worker, registered at birth, holding the `Handle` itself; listed by
  `workers`; governed by idle-observation leases.
- **The agent registry** (`exarch/src/agent_registry.rs`): every live agent as
  a tree; listed by `agents`, cancelled by `agent_cancel` with a subtree
  cascade; a `/clear` generation; a fixed one-hour ceiling per child.
- **The job table** (`ral/src/jobs.rs`): kernel-stopped process groups by
  pgid; `jobs`/`fg`/`bg`/`disown`; an exit sweep (SIGTERM, five seconds,
  SIGKILL) that `disown` exempts a group from.
- **The schedules registry**
  ([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]): cron and
  `after` wakeups, listable, cancellable by id, dead on `/clear`.
- **The binding ledger**
  ([[decisions/260629_agent-binding-reaping|agent-binding-reaping]]): leases
  over top-level scratch names, renewed by use.

Each solved listing, cancellation, expiry, and `/clear` locally, and each
restated a fragment of the same doctrine on its own page. The folds are
already duplicated — three listing verbs, two exit stories, two
generation-and-cascade disciplines — and `/resources` was about to become a
fourth independent enumerator.

### The wart, and a doctrine about to fossilize

The REPL has two backgrounding regimes with different semantics. `cmd &`
desugars to `spawn` ("pure sugar", `core/src/elaborator.rs`) — an in-process
handle that buffers output and is invisible to `jobs`. Ctrl-Z then `bg` yields
a tty-writing pgid *inside* `jobs`. A POSIX user expects `&` in `jobs`; ral
splits the expectation down the middle, and the job-table module doc has to
explain the split away.

The split was decreed when it was true:
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
ruled that the job table "is not a management layer over `&` or `spawn`" and
that job control "gains no spawn-handle analogue" — correct while detached
handles were unlisted by design. [[decisions/260705_leases-and-budgets|leases-and-budgets]]
then built `workers`, which *is* the spawn-handle analogue of `jobs`. The
separation doctrine needs restating before it hardens around a premise that no
longer holds.

## Decision

### A resident has four facets

- **Identity** — an id, with an owner edge into the session's tree (the agent
  tree, the shell that spawned the worker, the session that armed the
  schedule). Ownership is what cancellation cascades along.
- **Capability** — the typed value that reaches and controls it: a
  `Value::Handle` for workers, a pgid job spec for stopped groups, an
  `AgentId` for agents, a schedule id for wakeups, a top-level *name* for
  bindings. Capabilities are deliberately not unified; they are the honest
  variance between chapters.
- **Lease** — the lifetime row from
  [[decisions/260705_leases-and-budgets|leases-and-budgets]]: a clock, an idle
  bound, a renewal signal, an optional backstop — including the degenerate
  "none; legibility is the bound" (a `service`) and "none; a human owns it"
  (a stopped job).
- **Probe** — what it costs now, for the `/resources` fold.

Nothing session-lived escapes: a live thing with no chapter is a review
defect, the resident-shaped completion of the probe convention. The line
between a resident and a mere *accumulator* (a viewport, the bus, an inbox) is
the capability: a resident can be reached and controlled by id; an accumulator
can only be measured and bounded. Both are probed; only residents are listed,
cancelled, and leased.

### The ledger is an interface, not a struct

The chapters keep their own representations, locks, and homes — the worker
registry beside core's handles, the agent registry and schedules in exarch,
the job table in the REPL binary, per
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]].
What is shared is the small resident signature (identity, owner, population,
capability kind, lease row, probe row, cancel), defined once in core, and the
folds written against it:

- **list** — project a set of chapters into one table;
- **warn** — the exit story: what survives, what is swept, what is named;
- **cascade** — cancellation follows ownership edges regardless of population;
- **generation** — `/clear` bumps one generation all chapters observe, so
  anything settling across a clear is rejected, uniformly;
- **probe** — the `/resources` projection, over residents and accumulators
  alike.

Fusing the chapters into one registry struct is explicitly rejected: the
capability types differ, the lock disciplines differ (exarch's inbox→registry
order is load-bearing), and the lifetimes differ per chapter. The interface is
the unification; the struct would be the flattening.

### The residency order

Residency states are graded by independence from the session:

| grade | state | population | entered by | left by |
|---|---|---|---|---|
| 0 | foreground | the running turn | evaluation | completion, the wall, interrupt |
| 1 | stopped | pgid groups | kernel SIGTSTP | `fg`, `bg`, exit sweep |
| 2 | background | pgid groups (`bg`) · detached handles (`spawn`/`&`) | `bg` · birth | completion, `cancel`, lease reap, exit |
| 3 | durable | `service` workers | birth | completion, `cancel`, `/clear`, process exit |
| 4 | survives exit | disowned groups · future Regime 2 processes | `disown` · birth | outside the session's story |

Leases are the downward pressure — everything drifts toward reclamation unless
renewed. Explicit verbs move work up. One precision the informal name
"residency lattice" owes the reader: the structure is a *graded partial
order*, not a lattice — a `bg` pgid group and a detached handle sit at the
same grade with incomparable capabilities, and no join is claimed.

### Two traversal disciplines, one order

- **Interactive work discovers its lifetime.** A human cannot know at launch
  that this build wants backgrounding; intent is revealed late, so the REPL's
  verbs move work *mid-flight*: Ctrl-Z suspends, `bg` promotes, `fg` demotes,
  `disown` promotes to grade 4.
- **Agent work declares its lifetime.** A model states intent at birth —
  `spawn` is born at grade 2, `service` at grade 3 — and mid-flight movement
  (`promote`) stays deferred until a concrete need.

This dissolves an apparent contradiction rather than papering over it: *birth,
not promote* is not a universal law, it is the declaration discipline — and
promotion is not a rejected mistake, it is the discovery discipline's native
gait. `bg` **is** promotion. And `disown` **is** the survives-exit flip —
Regime 2's interactive ancestor, already shipping: removal from the job table
exempts a group from the exit sweep, and on Windows it literally clears
`KILL_ON_JOB_CLOSE`. When Regime 2 is built, it arrives with its obligations
pre-stated: present the four facets, occupy grade 4, and answer the probe.

### The correspondences decided

- **One listing fold.** REPL `jobs` gains the detached-handle population,
  marked by kind, healing the `&` wart at the layer users feel it. `fg` and
  `bg` remain pgid-typed verbs — a thread-backed handle has no SIGCONT — and
  the handle's own eliminators are the analogues: `await` is its `fg` (block
  the foreground until done, bytes replayed rather than streamed, no terminal
  handoff), `cancel` its kill. A settled-but-unclaimed handle job appears
  marked done until its result is observed, then leaves the listing — the
  analogue of POSIX printing `Done` at the next prompt — so an unclaimed
  result is never invisible in a host with no retention epoch. `workers` and `agents` stay the per-chapter
  projections in exarch; which chapters a host verb projects is host policy,
  that every chapter is *in* the fold is not.
- **One exit story.** Shell exit is a warn-then-sweep fold over the ledger:
  undisowned groups swept as today, live workers named — the survivor warning
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] deferred
  lands as a fold, and POSIX's "you have stopped jobs" is recognised as the
  same warning, not a cousin of it.
- **One cascade.** Cancelling a resident cancels what it owns, across
  populations: an agent's teardown reaches its workers through its shell's
  durable root, its schedules through `/clear`-and-cancel, its children
  through the registry — one law, per-chapter edges.
- **One generation.** The `/clear` generation the agent registry already
  keeps becomes the ledger's; a worker, schedule, or job settling across a
  clear is rejected by the same gate.
- **Enumeration is not observation** — restated here as ledger law: no fold
  renews a lease. Interest is naming a resident through its capability, never
  scanning past it.

### The fusion refused

The deep version — Ctrl-Z mints a `Value::Handle`, `fg` becomes `await`,
the job table dissolves into the worker registry — is rejected. The substrates
genuinely differ: pgid plus terminal versus thread plus result channel; a
kernel-driven state machine that can re-stop (SIGTTIN) versus a monotone
running-to-settled; tty inheritance versus bounded capture; and `fg` needs the
[[decisions/260619_terminal-lease|terminal-lease]], which is a host affordance
no eliminator should grow. Fusion would put a `Stopped` arm in `HandleState`
that only one host can produce and terminal semantics in `await` that only one
host honours — two implementations behind one name, the shape this family of
decisions has now rejected three times. The ledger is precisely what makes
refusing fusion affordable: unity lives in the signature and the folds, so the
capabilities can stay typed and distinct without the system falling into
pieces.

## Consequences

- **The wart heals where users feel it.** `sleep 10 &` appears in `jobs`,
  marked as a handle job; the POSIX expectation holds at the listing layer
  without a single change to spawn or pgid semantics.
- **Each law is stated once.** Exit, `/clear`, cascade, listing, and probing
  stop being per-page fragments; a new decision page never again needs to
  re-derive "what happens to these on clear?".
- **The order generates.** Parking an agent — stop scheduling its turns — is
  Ctrl-Z at the agent chapter, and the inbox park machinery is half of it
  already. `promote` is `bg`'s cousin with a reserved slot. Any proposed new
  resident type must answer four questions (identity, capability, lease,
  probe) or it does not ship.
- **Doctrine coherence.** "Unmanaged by default — nothing requires management,
  everything permits it" is said once, of the ledger, instead of once per
  registry.
- **The theory has a home to graduate to.** This page records the decision;
  once accepted and the first folds exist, the durable concept belongs in a
  `design/residency` page, with this ADR as its why-we-changed record.

## Alternatives considered

- **One registry struct.** Rejected: capability types, lock orders, and
  lifetimes all differ per chapter; a mega-registry flattens exactly the
  variance that is honest. The interface is the unification.
- **Deep fusion of job control into handles.** Rejected as above: two
  implementations behind one name, plus terminal authority leaking into an
  eliminator.
- **Status quo — five ad-hoc registries.** Rejected: the folds are already
  duplicated and drifting, the `&`/`jobs` split is user-visible, and 260616's
  separation doctrine has outlived its premise.
- **Making accumulators residents.** Rejected: a viewport has no capability;
  pretending it can be cancelled by id buys uniformity at the price of a lie.
  The probe fold spans both kinds; the other folds do not.

## Open questions — resolved or deferred, 260705

Every open question on this page was settled before implementation began.

- **`fg` for a handle job: no**, until a need is demonstrated. The
  terminal-lease handoff is imaginable; the fusion refusal holds regardless.
- **Agent parking: deferred**; its "what is its `fg`" sub-question defers
  with it.
- **The resident signature's home is decided by the parcel-9 extraction**,
  shaped by what the folds actually need — leaning a core trait, since the
  chapters span three crates and core owns two of them, under
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]'s
  constraint either way.
- **REPL `jobs` shows settled-but-unclaimed handle jobs, POSIX-`Done` style**
  — decided into the listing-fold correspondence above.

## See also

[[decisions/260705_leases-and-budgets|leases-and-budgets]] (the worker
registry, leases, and probes this generalises — its registry is the ledger's
first chapter), [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the handle model; the job-table separation restated here at the listing
layer), [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the
deferred survivor warning), [[decisions/260629_agent-binding-reaping|agent-binding-reaping]]
(the binding chapter), [[decisions/260617_scheduled-wakeups|scheduled-wakeups]]
(the schedules chapter), [[decisions/260619_terminal-lease|terminal-lease]]
(why `fg` is a host affordance), [[decisions/260614_structural-bug-prevention|structural-bug-prevention]]
(keep distinct things distinct by type — the capability argument),
[[map/repl/jobs|repl/jobs]], [[map/exarch/agent|agent]],
[[map/core/builtins|map: builtins]], and `docs/SPEC.md` §13 and §18.
