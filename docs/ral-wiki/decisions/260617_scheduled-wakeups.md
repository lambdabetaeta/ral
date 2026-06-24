---
status: proposed
---

# A wakeup schedules the agent, not a worker

**A scheduled wakeup is a timer that posts a synthetic user turn into the
agent's *inbox*, re-engaging the agent loop without a human present.** It
schedules the *agent* — the reasoning loop — not a *worker*. This is the line that keeps it
distinct from its sibling [[decisions/260617_long-running-work|long-running-work]]:
a `spawn`/born-durable worker runs a ral computation detached and yields a
*value* the agent later observes; a wakeup yields a *prompt* the model later
acts on. The timer runs no ral code — it manufactures a reason for the existing
turn driver to start a turn, and the model decides what that turn does.

The *when* is a **cron expression**, and almost nothing here is new code: the
mechanism is assembled from machinery already compiled into the binary.

## Context

exarch ([[design/exarch-architecture|exarch-architecture]]) is request–response.
Between turns it sits `ReadyForUser` ([[invariants/turn-ends-ready|turn-ends-ready]]),
blocked on a single source: a human prompt. To become a long-running agent —
especially when run **headless** as a resident process — it needs a *second*
reason to start a turn. The pieces a wakeup composes from already exist:

- **the delivery channel** — the bus `PromptQueue` (today a per-session
  `VecDeque<String>`) and its
  [[decisions/260616_tool-boundary-steering|tool-boundary-steering]] /
  turn-boundary drain (`exarch/src/bus.rs`, [[map/exarch/frontend|frontend]]);
  this is the channel that generalises into the *inbox* below;
- **the synthetic-prompt path** — `nudge` already manufactures a next prompt and
  feeds it through `append_user` (`exarch/src/nudge.rs`, [[map/exarch/agent|session]]);
- **timers as data** — `process::reaper` is a single daemon over a
  `BinaryHeap<Scheduled>` woken by a `Condvar`, firing one action at each entry's
  `when` (`core/src/process/reaper.rs`); it is the death-clock and the foreground
  wall of [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]];
- **calendar arithmetic** — `jiff` (timezone- and DST-aware) is *already a hard
  transitive dependency*, pulled in by the bundled `date` coreutil
  (`ral-core` → `uu_date` → `jiff-icu` → `jiff`). It is compiled into the binary
  whether or not this feature exists;
- **host-only affordances** — `register_builtins(EXARCH_BUILTINS)` adds verbs
  present in exarch and absent from core, the principle of
  [[decisions/260617_watch-repl-builtin|watch-repl-builtin]].

Only one piece is genuinely new (below): the back-channel that today carries
user steering must generalise into a *session inbox*, and the idle wait must
become multi-source over it.

### Why cron — and why it reuses rather than reinvents

Cron is *calendar* scheduling, and three facts make it the right surface:

- **A headless agent is long-lived, so calendar anchoring is common, not an
  edge case.** "Every weekday at 09:00", "nightly at 03:00", "on the half-hour"
  are the dominant shapes for a resident agent — exactly what cron expresses and
  a relative interval cannot.
- **Models are exceptionally fluent in cron.** A five-field expression is a
  lingua franca with vast training behind it; the model emits `0 9 * * 1-5`
  reliably. A bespoke `every`/`daily-at` mini-language is the *actual* wheel
  reinvention — novel surface with no training data, which the model fumbles.
- **The dependency objection is already spent.** Evaluating a cron expression
  ("when does this next fire, in the host's timezone, across a DST boundary?")
  needs a calendar library — and `jiff`, the modern one, is already compiled in
  via the bundled `date`. Declaring it in exarch's `Cargo.toml` resolves to the
  same built crate at zero marginal cost. (`host.rs` shells to `date(1)` to avoid
  a *direct* `chrono` dep for one string; the irony is that the `date` it sidesteps
  is what drags `jiff` in regardless.)

Cron is wall-clock; the reaper is monotonic `Instant`. The scheduler bridges the
two: `jiff` computes the next absolute occurrence in the host-local timezone, the
daemon sleeps the monotonic delta to it, and recomputes on each fire. DST shifts,
NTP steps, and machine suspend are absorbed by *recompute-on-fire* plus the
overlap-skip below (a long sleep past several occurrences fires at most once on
wake, not a burst).

## Decided

- **The payload is a prompt, not a computation.** A wakeup appends a synthetic
  *user message* — instructions in natural language — and the model decides what
  to do with the turn. It does not run a ral thunk. (Running a thunk detached is
  `spawn`'s job; running one *past the death-clock* is
  [[decisions/260617_long-running-work|long-running-work]]'s.) A worker produces a
  value; a wakeup produces a turn.

- **The trigger vocabulary is `cron` plus `after`.** Two forms, each expressing
  what the other cannot:

  | form | recurrence | semantics | substrate |
  |---|---|---|---|
  | `cron "<expr>"` | repeats | next calendar occurrence in host-local tz | `jiff` → monotonic deadline |
  | `after <dur>` | one-shot | a relative delay from now (`30m`, `2h`) | reaper's `Instant`, no calendar |

  `cron` subsumes recurring-calendar scheduling (`*/30`, ranges, lists,
  day-of-week names). `after` covers "in two hours", which cron *cannot* express —
  it has no now-relative offset. There is no bespoke `every`/`daily-at`.

- **Reuse, not reinvention** — the answer to "how much is new?":
  - **Evaluate cron with `jiff`** (already compiled): next-occurrence,
    timezone, DST. Declare it directly in exarch.
  - **Parse the five-field grammar in-tree** — it is small and fully specified —
    rather than pull a `chrono`-based cron crate (`cron`, `croner`), which would
    add a second datetime tree parallel to the `jiff` already present. (Revisit
    only if a mature *jiff*-based cron crate appears; the subtle parts —
    day-of-month/day-of-week OR-semantics, DST matching — are then worth not
    hand-rolling.)
  - **Fire on the reaper.** Generalise its entry's one action from "cancel a
    scope" to an action — `Cancel(scope) | Run(callback)`. The death-clock and
    wall stay `Cancel`; a wakeup is a `Run` that pushes the prompt and signals
    the wake. One timer daemon for the whole process; the `armed`/disarm guard is
    reused unchanged. Recurrence stays out of the reaper: entries remain one-shot,
    and the host re-arms the next occurrence on each fire.
  - **Deliver into the session inbox** — the generalised `PromptQueue` — reusing
    the `nudge` synthetic-prompt path. Cron is the inbox's first non-human
    producer; the delivery seam (below) is built once, for cron now and async
    workers later.

- **Ephemeral and per-session.** A schedule lives on the `Session`. It dies when
  the session ends and when `/clear` rebuilds the root
  ([[map/exarch/agent|session]]). There is no persistence, no boot-time reload,
  no operator config file. A `fork` does not inherit its parent's schedules —
  they are host state, not shell state, like the headless `expect_action` flag.
  Cron describes only the *when*; ephemerality is the *lifetime*, and the two do
  not conflict (a "09:00 daily" in a session that ends at 17:00 simply never fires
  again). A *durable* cron that survives restart is future work, the natural
  pairing with long-running-work's durable-job registry, not this page.

- **Delivery is at the turn boundary, not the tool boundary.** Tool-boundary
  steering ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]]) is
  for *user redirection* mid-turn. A scheduled task is a *fresh* turn and must not
  barge into the context of a turn already in motion. A wakeup drains through
  `Repl::drive` as the next prompt. The boundary is a per-message property of the
  inbox, not a global rule: user steering keeps the tool boundary, a wakeup takes
  the turn boundary. (An opt-in mid-turn `steer` trigger is a possible later
  refinement, not the default.)

- **At most one pending wakeup per schedule.** If a schedule's occurrence arrives
  while its previous wakeup is still unconsumed, the tick is *skipped*, not
  stacked — standard cron overlap semantics, and the same rule that bounds
  catch-up after a long suspend.

- **The registry is listable; cancellation is by id.** `schedules` lists the live
  schedules (id, expression, next fire, last fired); `unschedule <id>` removes
  one. This is load-bearing for the same reason it is in
  [[decisions/260617_long-running-work|long-running-work]]: auto-compaction loses
  the *binding name* that held the `ScheduleId`, so the model rediscovers its
  schedules by listing and acts by id. A live `ScheduleId` is pinned against
  [[decisions/260616_exarch-binding-reaping|exarch-binding-reaping]].

## The delivery seam: one inbox, many producers

The genuinely new piece is small and deliberately general, because a wakeup is
the first instance of a pattern the agent will meet again. Decompose any
"something happens to the loop later" into **`(trigger, effect, target)`** and
the seam is obvious — the three things below differ on *trigger*, but cron and a
finishing worker share an *effect* and a *target*:

| mechanism | trigger (*when*) | effect (*what*) | target |
|---|---|---|---|
| the reaper's wall / death-clock | time — a deadline | **cancel** a scope | a `CancelScope`, read at `process::check` |
| a cron wakeup | time — a recurrence | **post** a message · wake | a `Session` inbox |
| a finishing async worker (future) | event — the worker settles | **post** a message · wake | a `Session` inbox |

**So the unification is along the *delivery* seam, not a single queue.** Cron and
async-worker completion want the identical thing — *post a message to a session
and wake its idle loop* — so that becomes one mechanism:

- **Generalise `PromptQueue` (a `VecDeque<String>`) into a typed per-session
  inbox** — `VecDeque<InboxMsg>`, each message carrying its *source*
  (`UserSteering | ScheduledWakeup | AgentResult`) and its *drain boundary*. One
  **wake** signal accompanies it. This is the inbound twin of the existing
  outbound `Kind` event stream (`bus.rs`): same bus, opposite direction.
- **The idle wait becomes a select over `{ user input, inbox }`.** The reaper's
  `Run` action is one producer (cron); a finishing worker is another; the TUI is
  the third. `Repl::drive` pulls the next message and starts a turn as if the
  user had typed it. Build the inbox once, for cron now and async delivery later
  — the latter turns the canonical `spawn → poll across turns` idiom of
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
  into a push (*"your job finished"* arrives as a message).

Two seams that look adjacent must **not** collapse — each is a category error:

- **An event-trigger is not a timer entry.** A worker completion has no `when`; it
  fires on its own result channel (`HandleInner`'s `rx`). It posts to the inbox
  directly on settle — it is *not* armed as a zero-delay reaper entry, which would
  be a poll dressed as a deadline. The timer heap and the completion channel are
  different *triggers* that share only the inbox.
- **A cancellation is not a message.** The control plane (cancel a scope → read at
  `check` → means *unwind*) and the data plane (a message → read by the driver →
  means *here is work*) stay on separate rails. Collapsing them re-admits the
  abuse of treating a `Deadline` cancellation as a positive wake. Enforce it with
  a type: a cancellation must be *unconstructable* as an `InboxMsg`, in the spirit
  of [[decisions/260614_structural-bug-prevention|structural-bug-prevention]].

A shared channel does **not** mean a uniform drain policy: the boundary is the
per-message tag above, generalising
[[decisions/260616_tool-boundary-steering|tool-boundary-steering]] rather than
replacing it.

The two frontends realise the wait differently:

- The **TUI** already redraws each tick, so it polls the inbox cheaply; the wake
  signal need only interrupt its blocking input read.
- A resident agent wants a **daemon frontend** whose idle loop *is* this select —
  `headless.rs` today reads one seed and exits ([[map/exarch/frontend|frontend]]);
  the resident variant loops on the multi-source wait. Whether to build it now or
  lean on the TUI is open.

A wakeup renders as a *marked* user turn — e.g.
`[scheduled 'nightly' · 0 3 * * *] run the full test suite …` — so the model can
tell a wakeup from a human, and `events.json` records it as scheduled.

## Surface (illustrative)

`cron : String → Trigger`, `after : Duration → Trigger`,
`schedule : Trigger → String → ScheduleId`. The payload is a *string prompt*; the
cron expression is evaluated in the host's local timezone.

```
let nightly = schedule (cron "0 3 * * *")   "run the full test suite; summarise failures"
let standup = schedule (cron "0 9 * * 1-5") "post the standup digest to the channel"
let once    = schedule (after 2h)           "re-run the flaky integration test once"
schedules                                   # → the live schedules, by id
unschedule $nightly
```

Fixed arity holds ([[invariants/fixed-arity|fixed-arity]]). Self-scheduling is
gated behind a `schedule` authority in the [[design/grant|grant]] profile, off by
default — an agent that can wake itself indefinitely holds real authority, even
ephemerally.

## Alternatives considered

- **A bespoke trigger DSL** (`every <dur>`, `daily-at "09:00"`). Rejected: cron is
  the model-fluent lingua franca, and a custom mini-language is the actual
  reinvention — novel surface with no training data, fumbled by the model, and
  weaker than cron at the calendar cases a headless agent needs.
- **Pull a cron crate** (`cron`, `croner`) for parsing and evaluation. Rejected:
  both are `chrono`-based and would add a datetime tree parallel to the `jiff`
  already compiled in. Reuse `jiff` for evaluation and parse the small grammar
  in-tree instead.
- **Persist schedules / operator config across restart.** Out of scope by the
  ephemeral + per-session decision; a durable cron is future work, paired with
  long-running-work's registry.
- **A sibling timer daemon** (rather than generalising the reaper's action).
  Viable, and the lower-blast-radius option if touching a load-bearing core
  primitive is unwanted — but it duplicates the heap/`Condvar`/daemon the reaper
  already is. Generalising the action to `Cancel | Run` is the greater reuse, and
  a `Run` callback leaks no host representation into core: core fires opaque
  deadlines, the host supplies the closure.
- **A cron-specific delivery queue.** Rejected: a wakeup's effect — *post a
  message, wake the loop* — is identical to a finishing async worker's. Building a
  wakeup-only channel would force a second, parallel mechanism the moment async
  delivery lands. Generalise `PromptQueue` into one typed inbox now.
- **Default to tool-boundary steering.** Rejected: a scheduled task is a fresh
  turn, not user redirection; injecting it mid-turn pollutes an unrelated turn's
  context.
- **Inject a computation, not a prompt** (run a ral thunk on a timer). Rejected:
  that is `spawn` / [[decisions/260617_long-running-work|long-running-work]] — a
  detached worker producing a value. A wakeup re-engages the reasoning loop; its
  payload is a prompt the model acts on, not code the runtime runs.

## Open questions

- **The resident daemon frontend** — build the multi-source idle loop now, or
  rely on the TUI's tick until a headless-resident agent is actually wanted?
- **Hand-rolled matcher vs a vetted crate** — parse-and-match cron on `jiff`, or
  adopt a `jiff`-based cron crate if a mature one exists, to avoid subtle
  day-of-month/day-of-week and DST matching bugs.
- **Catch-up policy** — after a suspend past several occurrences, fire once
  (overlap-skip, the recommendation) or fire each missed occurrence.
- **Runaway bounds** — a per-session fire budget or minimum-interval floor beyond
  the `schedule` grant gate.
- **Compaction interaction** — a wakeup arriving mid-`compact`, and pinning the
  `ScheduleId` against [[decisions/260616_exarch-binding-reaping|binding-reaping]].
- **Grant for a scheduled turn** — the ordinary per-turn profile, or a distinct
  (perhaps narrower) scheduled-turn profile?
- **The inbox as its own `design/` page** — the `(trigger, effect, target)`
  decomposition and the inbox-as-inbound-bus span the reaper, cron, the bus, and
  the future async-worker delivery this ADR only anticipates. It may be worth
  filing as a cross-cutting concept page that predates that work.

## See also

[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the reaper / death-clock lineage and the deferred host-managed-job line),
[[decisions/260617_long-running-work|long-running-work]] (the sibling mechanism —
a durable *worker*, where this is a recurring *turn*),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (the core-mechanism /
host-affordance registration principle),
[[decisions/260616_tool-boundary-steering|tool-boundary-steering]] and
[[map/exarch/frontend|frontend]] (the prompt queue the inbox generalises and the
per-message drain boundary it carries),
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]] (the
missing-type discipline that keeps a cancellation unconstructable as a message),
[[decisions/260616_exarch-binding-reaping|exarch-binding-reaping]] (pin the
`ScheduleId`), [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]
(the frame, the root/foreground cancel scopes the control plane rides, and the
idle loop's home), [[design/exarch-architecture|exarch-architecture]], and
`docs/SPEC.md` §13.
