---
status: active
---

# Lifetime is a lease, residency is a budget

> Merges and supersedes [[decisions/260617_long-running-work|long-running-work]]
> and [[decisions/260630_long-session-resource-budgets|long-session-resource-budgets]];
> amends the detached-worker lifetime policy and the no-introspection doctrine of
> [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]].
>
> **Amended, 2026-07-06 — decided, not yet built:** the `workers` listing verb
> below is retired. Returning the registry as a language value was itself
> mislayered — the registry's own doc comment already assigned listing,
> reaping, and caps to the host and the lease layer, never this door — and
> every defect followed from that one mislayering: untransportable at the
> host seam (`SerialValue::from_ground` rejects the live `Value::Handle`s a
> listing carries), a soundness-compromised `∀α` scheme over a heterogeneous
> handle list, and a `cmd` field that only ever held one of three hardcoded
> placeholders. Legibility now splits by lease class, completing this page's
> own "length is declared at birth" principle: the worker class gets no
> model-facing listing at all, its idle-observation lease bounding the harm
> of a forgotten spawn directly; the durable class's bound, already
> legibility, becomes a host-owned pinned ledger row — never a computed
> value — and `service` gains a mandatory `description`. `service-handle
> <id>`, read off that row, is the narrow door back to a never-bound
> service's handle. The sections below are revised in place; the retired
> design moves to Alternatives considered.

**A long-running exarch is threatened by two different kinds of growth: work
that lives too long unseen, and state that accumulates unaccounted. This
decision bounds each with one discipline. Every lifetime becomes a *lease* — an
idle bound on a named clock, with a named renewal signal and an optional
absolute backstop — so that "abandoned" finally means what it should:
*unobserved*, not *old*. Every session-lived accumulator becomes a *budget* — a
cap, a pressure policy, and a registered probe — so that nothing can grow
without saying so. The hinge between the two is a worker registry: every
detached worker is registered at birth, which retires the doctrine that
detached workers are unmanaged by design, and lets one mechanism serve
abandonment, durability, rediscovery, and accounting at once.**

## Context

### What the merged pages held

[[decisions/260617_long-running-work|long-running-work]] designed the escape
from the one-hour detached-worker death-clock: a long job is *born* durable —
a distinct exarch-registered verb birthing a worker into a listable
durable-job registry — never an ordinary `spawn` promoted after the fact. Its
headline question was left open: build the in-process regime (Regime 1), the
survives-exit regime (Regime 2), or both.

[[decisions/260630_long-session-resource-budgets|long-session-resource-budgets]]
established that a days-long run is not bounded by model compaction but by
ordinary heap, queues, shell values, and log files, and proposed per-accumulator
policies: a bounded presentation bus, windowed viewports, inbox quotas without
silent loss, physical reclamation after compaction, and a `/resources`
diagnostic.

Both pages were right about their own subject. Merging them exposed a shared
foundation neither could build alone, and one doctrine both had already
quietly repealed.

### The doctrine three pages already repealed

[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
declared: "detached workers are unmanaged by design — there is no listing,
inspecting, or enumerate-and-kill primitive." Given unfindable workers, a hard
age ceiling was the only safe lifetime policy, and the one-hour death-clock
was correct. But the premise did not survive its own successors:

- [[decisions/260617_long-running-work|long-running-work]] reserved a listable
  registry with cancellation by id — introspection for the durable class.
- [[decisions/260630_long-session-resource-budgets|long-session-resource-budgets]]
  wants `/resources` to report running and settled handles per agent —
  introspection for the operator, over *every* worker.
- exarch already runs the full pattern at the agent layer: the
  [[map/exarch/agent|fleet]]'s `AgentRegistry` (`exarch/src/agent_registry.rs`)
  gives every sub-agent a listing (`agents`), cancellation by id
  (`agent_cancel`), a subtree cascade, a `/clear` generation, and a one-hour
  ceiling (`AGENT_CEILING`, `exarch/src/agent_registry.rs:46`) — and its module
  doc must explicitly disclaim being the durable-job registry.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] had already
  named and deferred a **worker registry** — handle id, display name, cancel
  scope, deadline — for a survivor warning it could not otherwise print.

Four pages, four partial registries. The doctrine was repealed piecemeal;
nobody wrote it down. This page does: **the registry is built once, for every
detached worker**, and the lifetime policy is re-derived on top of it.

### The death-clock conflates long-running with abandoned

The death-clock is armed by `spawn_child`
(`core/src/builtins/concurrency.rs:277`): under a frame that supplies a
detached ceiling (`Shell::turn.detached_ceiling`,
`core/src/types/shell/mod.rs:266`), the worker's scope goes to the shared
reaper as a kept, fire-and-forget entry. exarch supplies one hour
(`DETACHED_WORKER_CEILING`, `exarch/src/shell_eval.rs:26`, threaded at
`shell_eval.rs:268`); the REPL supplies none. The ceiling fires on wall-clock
age, full stop.

That shape kills the wrong thing. The canonical long-work idiom is `spawn` →
`poll` across turns → `await` once settled, and
[[decisions/260702_partial-poll-pending-output|partial poll]] made `poll` a
genuine read — a pending poll returns the worker's accumulated output. A model
babysitting a three-hour build, polling it every turn, loses it at hour one
anyway. The death-clock cannot tell that worker from a forgotten
`spawn { loop { … } }` whose binding name compaction erased. It conflates
*long-running* with *abandoned* because, without a registry, age was the only
observable fact.

[[decisions/260629_agent-binding-reaping|agent-binding-reaping]] had already
articulated the correct principle for the sibling problem: creation-age expiry
was rejected for bindings because "an old but active definition is not
garbage" — *use renews the lease*. The principle was simply never applied to
workers.

### What bounds a days-long host

The pressure points from the superseded budgets page, re-anchored:

- The TUI keeps a live graphic transcript; viewports and retained dead-agent
  views grow with the session.
- The session-lifetime bus
  ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]])
  is an unbounded channel; producers outrunning the renderer accumulate heap.
- Per-agent inboxes are unbounded `VecDeque`s (`exarch/src/bus.rs:381`)
  drained only at tool and turn boundaries; they carry model-facing facts, so
  they cannot be treated like presentation.
- Compaction is not reclamation: the in-memory event mirror and the durable
  files (`events.json`, `transcript.jsonl`, `user.log`) can retain the
  historical prefix after the model view shrinks.
- The lexical env is cheap to clone, not cheap to fill: the resident cost is
  the `Value` — large strings, closures capturing old scopes, and handles
  retaining buffered output.
- Some buffers are bounded only multiplicatively: each captured sink caps at
  16 MiB (`SINK_BUFFER_CAP`, `core/src/io/sink.rs:21`), but many handles or
  agents still add up.
- The foreground wall is not the bound that pinches: a synchronous `ral` call
  defaults to 60 s and is raisable per call (`CALL_TIMEOUT_SECS`,
  `exarch/src/tools/ral.rs:38`); long work already escapes it via the spawn
  idiom.

The failure mode over days is not one large thing; it is many small
session-lived accumulators with no common accounting — and, once work runs
long, a lifetime policy that reaps the work being watched.

## Decision

Two principles, one mechanism. The registry comes first because both
principles lean on it.

### The worker registry

**Every detached worker is registered at birth.** Core supplies the mechanism;
hosts register the affordances
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]'s principle,
already reused once for the agent tools).

- **Core-owned, per-shell.** The registry lives beside the binding-lease
  ledger on the shell's host-local scratch, per
  [[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]:
  exarch receives operations (list, count, per-entry facts), never the
  representation. One registry per `Shell` means one per agent, matching the
  binding ledger's per-agent rule; the fleet view is a fold over agents.
- **An entry is minted at `spawn_child`.** Handles carry no identifier today
  (`HandleInner`, `core/src/types/value.rs:317`); the registry mints a stable
  id and records it with the worker's spelling (`cmd`), start time, lease
  class, and — decisively — **the `Handle` itself**.
- **The registry stores handles; there is still no general by-id control
  plane over them.** `poll`, `await`, `race`, and `cancel` remain the only
  verbs that touch a running worker, and the handle remains the only
  capability. Rediscovery after a compaction erases a binding name no longer
  runs through a listing — amended, 2026-07-06: the binding lease already
  keeps any *named* handle of running work alive forever
  ([[decisions/260629_agent-binding-reaping|agent-binding-reaping]]'s
  `pins_running_work` — a binding whose value reaches a running worker is
  never pruned, and bindings are shell state that survives compaction), so a
  babysat worker's own name is the durable rediscovery path, not an
  enumeration. The residual gap is a service that was never bound to a name
  at all; **`service-handle <id>`**, reading the id off that service's host-pinned
  ledger row, is the one narrow door back to its handle — built for the
  durable class alone, because only it carries a legibility structure to
  read an id off. This preserves the healthy half of the old doctrine — the
  concurrency page's own thesis was "the handle is the evidence of
  detachment" — without reopening the by-id control plane rejected below:
  `service-handle` mints no second `poll`/`cancel`-by-id surface, it only
  re-establishes the binding a name would ordinarily hold, after which the
  ordinary eliminators apply.
- **Legibility splits by lease class — amended, 2026-07-06.** Exarch does not
  register a listing verb over the registry; the registry exists in every
  shell regardless (it is cheap), but exposing it as a language value was the
  mislayering this page's own worker-registry doc comment already warned
  against. The worker class gets no model-facing listing at all: its
  idle-observation lease already bounds the harm of a forgotten spawn to at
  most an hour of one seat out of the per-agent cap, so a rail card at birth
  and a reap card at death are the entire legibility story, carried by the
  tool call's own mandatory `description` and the transcript reap event
  below — no reap notice needs to ride a model-facing channel on top of
  that. The durable class's bound was always legibility; it is now a
  **host-owned pinned ledger row** per live service — id, description, age —
  rendered from the registry and re-injected across compaction like any pin,
  never computed by the program as a value. `service` gains a **mandatory
  `description` argument** to fill that row (exarch-only, so no cross-host
  signature question arises). The REPL keeps its POSIX job control
  ([[map/repl/jobs|repl/jobs]]) untouched either way — it never had
  `workers` — and the survivor warning
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] deferred
  is served by the same reap-card mechanism, not a listing consumer.
- **Entry lifecycle.** A `Running` entry is governed by its lease (below). On
  settle the entry remains as a *settled* record under a short retention
  lease: the registry is where an unclaimed result waits, out of band — the
  completion-payload home the budgets page asked for, converging with
  [[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]],
  where the inbox carries only the bounded "spawn finished" wakeup. A settled
  entry is removed when its result is observed or its retention lease expires;
  a reaped or cancelled entry is removed at once. The retention is a deliberate
  residency cost: a settled entry keeps its cached result alive independently
  of any binding, so pruning a top-level name defers reclamation of a large
  result to the retention lease — bounded by that lease, the per-agent worker
  cap, and the per-handle sink caps. Every removal by policy
  emits a compact transcript event, symmetric with binding-prune events —
  the model's later "where did my job go?" always has an answer in the log.
  Model-visible delivery of a reap is settled, not deferred — amended,
  2026-07-06: the worker class's whole legibility story is the rail card at
  birth and the reap card at death, both host-facing, and the
  idle-observation lease already bounds the cost of the model never being
  told directly (at most an hour, one seat of the per-agent cap). No reap
  notice needs to ride a model-facing channel on top of that, so
  [[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]]'s
  general completion-notice design is left for other channels, not required
  here; this page's reap events stay transcript/TUI-only, now by decision
  rather than by default.
- **`/clear` outranks every lease.** It already rebuilds the focused agent's
  shell and cascades cancellation through its subtree; it now also cancels
  that shell's registered workers, the durable class included. Explicit
  destruction is the one gesture stronger than durability. The registry
  reuses the `/clear` generation discipline agent results already obey: a
  worker settling across a `/clear` is rejected, not delivered.

### Principle one: lifetime is a lease

A lease is an idle bound on a **named clock**, renewed by a **named signal**,
under an optional **absolute backstop**. A lifetime policy that cannot name
its renewal signal is a guess. The existing lifetimes, restated:

| lifetime | clock | idle bound | renewal signal | backstop |
|---|---|---|---|---|
| detached worker (today) | wall | 1 h from birth | **none** | — |
| detached worker (this page) | wall | 1 h unobserved | observation | 24 h |
| durable worker (this page) | — | none | — | none; legibility is the bound |
| binding lease ([[decisions/260629_agent-binding-reaping|260629]]) | ral-call epoch | 256 idle calls | use in a committed turn | — |
| sub-agent ceiling | wall | 1 h from birth | none — deliberately (see below) | — |
| foreground wall | wall | 60 s | — (a wall, not a lease; disarmed on completion) | — |

- **Ordinary `spawn` gets an idle-observation lease.** The worker is reaped
  when *unobserved* for one hour, not when one hour old. Observation is an
  eliminator naming the handle — `poll`, `await`, `race` — each of which
  records a touch on a shared last-observed cell the worker's registry entry
  reads. This needs no boundary harvest and no hot-path compromise: unlike
  `Env::get`, whose purity forced
  [[decisions/260629_agent-binding-reaping|agent-binding-reaping]] into static
  turn-scoped sets, the eliminators are already effectful builtins holding the
  handle's own locks; one more touch is free. Because the cell lives with the
  handle, observation renews from anywhere the handle travels — a parent's
  binding, a child's forked scope, a closure capture.
- **Enumeration is not observation.** `/resources` and the survivor warning
  never renew a lease, and neither does rendering the durable class's pinned
  ledger row — amended, 2026-07-06: none of these is a model-facing listing
  to begin with, now that the worker class carries no listing at all.
  Interest is *naming* a worker through an eliminator, or a `service-handle`
  call, never scanning past it; if rendering a pin renewed anything, the
  operator's own diagnostics would immortalise every zombie they exist to
  reveal.
- **The backstop stays.** An ordinary spawn also carries a 24-hour absolute
  ceiling the model cannot extend by ritual polling. The host keeps a hard
  bound on the construct that is *supposed* to be bounded; work that
  legitimately outlives it is durable by intent and should be born that way.
- **The mechanism is already built.** The reaper's `Run` entries re-arm
  themselves (`process::arm_callback`, proven by `callback_can_rearm_itself`,
  `core/src/process/reaper.rs`) — the exact shape an idle lease needs: at
  fire time, compare the last-observed cell; renewed means re-arm for the
  remainder, idle means cancel with the `Deadline` cause. The kept `Cancel`
  entry at `concurrency.rs:277` becomes a kept `Run` entry holding the scope
  and the cell. The REPL continues to arm nothing.
- **Born durable is a lease class, not a machinery stack.** The
  exarch-registered verb — **`service { … }`**, settling the superseded page's
  spelling question (`daemon` mis-suggests surviving process exit; `disown`
  and `job` are REPL job-control vocabulary) — births a worker whose registry
  entry carries the durable class: no idle bound, no backstop. Its bound is
  *legibility*: amended, 2026-07-06, a **host-owned pinned ledger row** (id,
  description, age) rendered from the registry and re-injected across
  compaction like any pin — never a listing verb, never a value the program
  computes — visible in `/resources`, cancellable through its handle, dead
  with the process. `service` now takes a mandatory `description` alongside
  its thunk, to fill that row. A backstop on the construct whose purpose is
  outliving ceilings would re-import the problem it solves; visibility
  replaces mortality, and visibility is now structural rather than queried.
  Everything else about the worker is ordinary — a real `Handle`, the same
  eliminators, the same registry — which is what the superseded page's
  Regime 1 wanted, delivered as one enum value instead of a parallel
  subsystem.
- **Birth, not promotion — for intent, no longer for mechanism.** A server or
  a multi-hour run is known to be long at launch, so the durable class is
  declared at birth; that argument stands. The superseded page's *mechanical*
  argument — that promotion would need a side registry of `Deadline` guards
  solely to disarm ceilings — dissolves, because the registry now holds every
  worker's lease state anyway. That is a feature: if a concrete mid-flight
  need ever appears, `promote` is one policy flip on an existing entry. It is
  still not built until that need appears.
- **Regime 2 stays out, unchanged.** Surviving exarch's exit is not a handle
  concept — an in-process thread cannot outlive its process. It remains a
  separate, later process-detachment capability (`setsid`, IO to files,
  reconnection by pid), pursued only on concrete need, never fused with
  `service`.
- **The binding reaper is untouched; durable pinning dissolves.**
  [[decisions/260629_agent-binding-reaping|agent-binding-reaping]] stands
  exactly as designed — running handles still pin the names that reach them,
  settled handles are still scratch. The running-handle pin survives on a
  shifted ground: it is no longer what keeps a worker reachable — the registry
  is — but 260629's own rule that a lease policy must never surprise-delete
  the name of live work. But because the registry retains the
  handle itself, pruning a top-level name can never strand a job, so the
  superseded page's requirement that durable jobs be pinned against the
  binding reaper is deleted, and 260629's open question — should host pins
  name bindings or ids — is answered: *neither*; the registry holds the
  handle, and no host pins are needed for durable work.
- **Sub-agents keep a fixed lease — deliberately.** The idle-observation
  critique does not transfer: a sub-agent is push-delivery (`reply` into its
  parent's inbox) and self-terminating, so a healthy child is unobserved for
  its whole run — unobservedness is not its abandonment signal, and
  `AGENT_CEILING` keeps its fixed one-hour shape. What changes is legibility:
  the agent ceiling becomes a row in the same lease table, reported by
  `/resources`, instead of a constant that happens to equal the worker
  ceiling. Extending a known-long child's budget is future work, listed open.

### Principle two: residency is a budget

The superseded budgets page's per-accumulator decisions are carried forward
whole; they were right. Restated tightly, with what this page adds:

- **Context budget and host budget stay separate.** `/compact` is a provider
  context operation and must not be sold as memory reclamation; heap and disk
  get their own machinery and diagnostics.
- **The presentation bus is bounded by class.** Lifecycle events (`Born`,
  `Died`, terminal tool-result frames) are reserved; tokens, phases, progress,
  and high-frequency card updates may coalesce. An elided class leaves one
  explicit overflow marker naming the class and count — degradation the user
  sees, never silence.
- **Viewports keep a window.** The current reply, pins, and the last N blocks;
  older blocks are already durable in `user.log`/`events.json` and are not
  duplicated in heap forever. Dead sub-agent viewports are flushed, then
  evicted after a linger into a tombstone carrying id, status, and log path.
- **Compaction physically drops the model prefix in memory.** After a
  successful compaction the in-memory event vector matches the live model
  view — summary plus suffix. The append-only files remain the forensic
  record; reclamation is for heap, not history.
- **Inboxes get quotas without silent loss.** Idempotent sources coalesce
  (schedule wakeups by id, repeated steering before a boundary, duplicate
  completion nudges); non-idempotent facts are accepted or *rejected with a
  user-facing error*, never dropped. Completion payloads now have their
  concrete out-of-band home: the worker registry holds the settled result,
  the inbox carries the bounded wakeup.
- **Shell residency is lexical state plus host leases.** The binding reaper
  covers scratch names; large bindings draw a soft-threshold warning that
  recommends file paths over captured bytes. A closure keeping a value alive
  after its name is pruned is lexical scope working, not a leak.
- **New — the probe convention.** Every session-lived accumulator registers a
  **probe** at construction: its name, current size, cap, and pressure policy
  (coalesce, reject, evict, reap). `/resources` is a *fold over registered
  probes* — per fleet and per agent: bus depth, inbox depth per source,
  viewport blocks/rows/bytes, event-vector length, live and dead viewports,
  binding count with large bindings by rough retained size, running and
  settled registry entries with lease class and time-to-reap, and log/scratch
  disk use. This turns the old page's prose principle — "every session-lived
  thing must say what bounds it" — into a checkable convention: an accumulator
  without a probe is a review defect, and a budget that cannot be inspected
  will be debugged by restarting the process.
- **New — the registry is an accumulator too.** Live workers per agent are
  capped, generously; at the cap, `spawn` *rejects* with a diagnostic naming
  the remedies (await, cancel) rather than admitting work the host cannot
  afford — the inbox rule applied to admission. This is per-agent, so
  it does not couple sibling branches' budgets, the ground on which
  [[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]] rejected a
  fleet-wide agent cap. Admission pressure is rejected; abandonment is
  reaped; the two pressures get the two different answers they deserve.

### Suggested defaults

First-implementation numbers, all visible in `/resources`, none buried:

- **Worker lease:** 1 h unobserved; 24 h absolute backstop.
- **`service`:** no idle bound, no backstop; dies with the process or by
  explicit cancel or `/clear`.
- **Settled-entry retention:** 256 idle ral calls, matching the binding
  lease's scratch expiry.
- **Live workers per agent:** cap at 64; reject at the cap.
- **Viewport:** cap by blocks and rendered rows; evict old blocks before
  retaining old dead-agent views.
- **Bus:** reserve lifecycle and final frames; coalesce tokens and progress
  per agent.
- **Inbox:** cap per agent and per source; reject non-idempotent overflow.
- **Disk:** report session-log and scratch size; warn at an operator-set
  ceiling.

## Consequences

- **Babysat work lives; forgotten work dies.** A worker polled across turns is
  renewed indefinitely up to the backstop; an unpolled worker is reaped in an
  hour, exactly as today. Abandonment now decays in layers, each with its own
  lease and its own log line: an hour unobserved reaps the worker; the
  `Cancelled` handle unpins its name into ordinary scratch; 256 idle calls
  later the binding lease prunes the name. The model's later
  `undefined variable` always has a paper trail.
- **Compaction stops being lethal to long work — for named work, and for
  services by a narrower door.** Losing a binding name never loses a worker
  that was ever bound: the binding lease keeps a named handle of running work
  alive regardless of compaction, so re-reading the name resumes polling.
  Amended, 2026-07-06: a service that was never bound at all is the one case
  compaction can strand, and it is not stranded either — `service-handle <id>`
  off the durable class's pinned ledger row hands the handle back. The
  registry,
  not the lexical scope, is the durable home for a running worker's identity.
- **The old doctrine is retired with a precise replacement, twice over.** Not
  "detached workers are unmanaged by design" but: *detached workers are
  unmanaged by default — nothing requires management, everything permits
  it.* Amended, 2026-07-06: permission itself is now graded by class — the
  worker class permits no management beyond its own eliminators, the lease
  being the whole story; the durable class permits legibility structurally,
  through a pin, never a queried value. The language gains no ambient
  authority: `service` is a host-registered affordance like `watch`, and a
  bare ral host has a registry no verb exposes at all.
- **One vocabulary where there were four mechanisms.** The death-clock, the
  binding ledger, the agent ceiling, and the durable-job registry become rows
  of one lease table and entries of one registry pattern; the two independent
  one-hour constants stop being a coincidence and start being a policy. The
  worker registry is the first chapter of the horizon this points at — the
  session ledger of [[decisions/260705_session-ledger|session-ledger]], where
  workers, agents, stopped jobs, schedules, and bindings are populations of
  one interface and the management surfaces are folds written once.
- **Overflow becomes a UI state, and model-facing delivery stays honest** —
  carried from the budgets page: elision is marked, inbox pressure is a
  rejection or a coalesced wakeup, never invisible loss.
- **Headless and TUI diverge only in presentation budgets.** Headless has no
  viewport pressure but the same registry, leases, inboxes, event vectors,
  and disk.
- **Memory reclamation stays intentionally incomplete.** Pruning a name or an
  event prefix may not free a captured value; the diagnostic explains
  reachability rather than promising retained size.
- **The survivor warning becomes cheap.** The registry
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] wished for
  exists; a REPL Ctrl-C can name its surviving workers whenever the REPL
  chooses to consume it.

## Supersession ledger

- **[[decisions/260617_long-running-work|long-running-work]] — superseded.**
  Its headline question is answered: Regime 1, delivered as the durable lease
  class of the universal registry; Regime 2 deferred unchanged. Its settled
  points survive inside this page: birth not promotion (intent argument), a
  distinct exarch-registered verb (now spelled `service`, now with a
  mandatory `description`). Listability does not survive as a universal
  listing verb — amended, 2026-07-06: the worker class carries none at all,
  and the durable class's listability is a host-owned pinned ledger row, not
  a queried value. Its cancel-by-id question is answered narrowly, not
  dissolved: `service-handle <id>` re-acquires a never-bound service's handle
  off that row; its pinning requirement dissolves as originally argued (the
  registry holds the handle regardless).
- **[[decisions/260630_long-session-resource-budgets|long-session-resource-budgets]]
  — superseded.** Every per-accumulator decision carries forward; this page
  adds the probe convention, the registry as the workers' accounting spine and
  completion-payload home, and the per-agent admission cap.
- **[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
  — amended, not superseded.** The root/foreground split, root-parented
  handles, `await`/`race` unification, `forget`'s deletion, `watch`'s
  host-registration, and the shared reaper all stand. Amended: the
  death-clock's creation-age policy becomes the idle-observation lease, and
  the "no introspection" doctrine is retired as above.
- **[[decisions/260629_agent-binding-reaping|agent-binding-reaping]] —
  untouched, one question answered.** Host pins for durable jobs are
  unnecessary; the registry retains the handle itself.
- **[[decisions/260617_scheduled-wakeups|scheduled-wakeups]] — untouched, one
  pin dissolved.** A live `ScheduleId` needs no pin against the binding
  reaper (and its open compaction-interaction question closes with it): the
  schedules registry is the authority and `schedules` re-lists after
  compaction, exactly as the worker registry dissolved the durable-job pins.

## Alternatives considered

- **Keep the creation-age death-clock.** Rejected: it reaps the babysat build
  it cannot distinguish from a zombie, violating the renewal principle
  [[decisions/260629_agent-binding-reaping|260629]] established for bindings.
  Its justifying premise — workers are unfindable, so age is the only signal —
  is repealed by the registry that durable jobs and `/resources` independently
  require.
- **A `timeout:` knob on ordinary `spawn`.** Still rejected, inherited from
  both predecessors: `spawn` stays one boring primitive under host-owned
  policy; long life is a declared class, not a dial.
- **Promotion instead of birth.** Still rejected for intent — length is known
  at launch. The old mechanical objection no longer does the arguing, and
  deliberately so: a future `promote` is one policy flip if a concrete
  mid-flight need ever appears. Not built speculatively.
- **Tie worker lifetime to name reachability** (the reachability-GC idea once
  parked under `forget`). Rejected: lexical reachability is not interest — a
  handle captured in a dead closure is exactly the zombie to reap — and
  chasing `Arc<Env>` capture graphs is the analysis 260629 explicitly refused.
  Observation is the honest signal.
- **Renewal on listing.** Rejected: enumeration is not interest; the
  diagnostics would keep alive everything they exist to expose.
- **A by-id control plane** (`cancel <id>`, `poll <id>`). Rejected: it would
  mint a second authority over workers beside the handle and a second set of
  verbs beside the eliminators. Amended, 2026-07-06: `service-handle <id>` is
  a deliberately narrow exception, not a reopening of this rejection — it
  re-acquires a never-bound service's handle off its host-pinned ledger row
  and hands back the capability itself; every operation after that still
  goes through the handle and the ordinary eliminators. The capability story
  stays single.
- **Fuse `service` with survives-exit work.** Still rejected: different
  observability (handle versus pid and exit status), different lifetime,
  almost no shared code — one name over two implementations.
- **Admission caps instead of leases** (reject new spawns at a cap; never
  reap). Rejected as a replacement, adopted as a complement: a stuck worker
  never completes, so a cap alone converts one zombie into a starved agent.
  Admission pressure and abandonment are different pressures.
- **Rely on `/compact` and `/clear`; let the OS be the budget; auto-clear old
  state; keep the bus unbounded because it is only presentation.** All
  rejected, inherited from the budgets page: a context operation is not a
  resource policy, process death loses live state that logs cannot resume,
  clearing changes semantics, and unbounded presentation can still kill the
  authoritative loop.
- **Keep `workers` as a language-level listing builtin.** Rejected,
  2026-07-06: its defects were not roughness to sand down but consequences of
  one mislayering. The registry's own doc comment already assigned listing,
  reaping, and caps to the host and the lease layer, never this door; a
  returned listing is untransportable at the host seam
  (`SerialValue::from_ground` rejects the live `Value::Handle`s every entry
  carries); it forced a soundness-compromised `∀α` scheme over a
  heterogeneous handle list; and its `cmd` field only ever held one of three
  hardcoded placeholders (`"<block>"`, `"<watch>"`, `"<service>"`) — zero
  identifying information. Retired rather than patched.
- **Give the worker class the same host-pinned ledger row as the durable
  class.** Rejected, 2026-07-06: the worker class's idle-observation lease
  already bounds a forgotten spawn's cost to at most an hour of one seat out
  of the per-agent cap; a structural affordance would buy legibility the
  class does not need to pay for. The durable class earns its row precisely
  because it arms no chain that would otherwise bound it.

## Open questions — resolved or deferred, 260705

Every open question on this page was settled before implementation began.

- **The four lease numbers are confirmed as decided**: 1 h idle TTL, 24 h
  backstop, 256-idle-call settled retention, 64 live workers per agent. The
  presentation caps (blocks, rows, bus depth, inbox depth per source, disk
  warning size) remain implementation-picked, visible in `/resources`.
- **Viewport eviction: a tombstone with the log path is enough.** No
  reload-from-`user.log` machinery is built; the log stays readable outside
  the TUI.
- **Disk: report and warn only.** Forensic records are never auto-rotated or
  auto-deleted, neither live nor at session boundaries; cleanup is a human
  act.
- **Retained size: shallow per-binding estimates only**, never `Arc`-graph
  walking — the same refusal
  [[decisions/260629_agent-binding-reaping|agent-binding-reaping]] made.
- **Transport-parametric budget negotiation: deferred** until the
  [[decisions/260628_host-seam-transport-parametric|transport-parametric frontend]]
  exists.
- **Parent-extendable `AGENT_CEILING`: deferred** until a concrete long child
  appears. Ceiling-less branch children (`dev/docs/260705_branch_minimal.md`)
  already prove the lease-class move at the agent layer, so the shape is
  known when the need arrives.
- **No host gets a `workers` verb, as of 2026-07-06** — not the REPL, and no
  longer exarch either: the `jobs` fold and the survivor warning
  ([[decisions/260705_session-ledger|session-ledger]]) are the REPL's whole
  surface; exarch's worker-class surface is a rail card at birth and a reap
  card at death, and its durable-class surface is the pinned ledger row plus
  `/resources`. No host lists the raw registry as a language value.

## See also

[[decisions/260705_session-ledger|session-ledger]] (the generalisation: the
registry as the first chapter of one ledger of residents),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the handle model and death-clock this amends),
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the
root/foreground split; the registry it deferred),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (core mechanism,
host affordance), [[decisions/260629_agent-binding-reaping|agent-binding-reaping]]
(the lease principle, applied here to workers),
[[decisions/260702_partial-poll-pending-output|partial-poll-pending-output]]
(observation as a genuine read),
[[decisions/260622_sync-surface-async-notify|sync-surface-async-notify]]
(push completion riding the inbox),
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]],
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]],
[[decisions/260617_scheduled-wakeups|scheduled-wakeups]],
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]] (the depth budget;
its per-branch cap argument), [[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]],
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]],
[[invariants/probe-convention|probe-convention]] (the probe bullet as a
checkable rule), [[map/exarch/agent|agent]], [[map/exarch/frontend|frontend]],
[[map/core/shell-state|shell-state]], [[map/core/builtins|map: builtins]],
[[map/repl/jobs|repl/jobs]],
[[internals/output-capture-and-detachment|output-capture-and-detachment]], and
`docs/SPEC.md` §13.
