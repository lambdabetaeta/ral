---
generated_at_commit: 50388d83
generated_at_date: 2026-08-29
covers_paths: [exarch/src/agent.rs, exarch/src/agent/, exarch/src/fleet.rs, exarch/src/fleet/desk.rs, exarch/src/fleet/roster.rs, exarch/src/prompt.rs, exarch/src/config.rs, exarch/src/net_policy.rs, exarch/src/net_policy/, exarch/src/egress.rs]
---

# Map: exarch / agent

`agent.rs` splits the node every fleet member is into two types along who
may touch what ([[decisions/260827_agent-and-avatar|agent-and-avatar]]).

**`Agent` is the public half**, held behind an `Arc` the fleet shares:
identity (`id`, `name`, `log_dir`), the two cancel doors (`cancel::Token`
and the eval-layer `reach: EvalReach`), the sender half of its own
`mailbox`, an owned hot-swappable `provider: ProviderHandle`, immutable
config (`caps`, `fuel`, `returns`, `search`, `interactive`,
`allow_schedule`, `egress`, `dial`, `index`, the resolved `system` prompt,
`disk_warn_bytes`), the single-writer `status: Mutex<Status>` — `{
rest, reply, awaiting }` — `consumer` (the parent's `Stamp`, minted at this
agent's birth; `None` for a root), a strong `parent: Option<Arc<Agent>>`,
and a weak `children: Mutex<Vec<Weak<Agent>>>`.  There is one fence, the
inbox's own clear-epoch
([[decisions/260827_agent-and-avatar|agent-and-avatar]]), and it is carried
only inside `bus::Stamp` — the addressed envelope `Mailbox::stamp` mints,
pairing the destination mailbox with its epoch at composition — so a
message that cannot judge its own staleness (`bus::Stamped`) is unsendable
with a stamp forgotten, skewed to another inbox, or refreshed at push time;
the destination judges it at its own pop.

**`Avatar` is the private half**: the canonical
[[internals/session-record|model projection]] (`AgentLog`), a **seat**
(`agent/seat.rs`) carrying the transport this run drives through —
`Seat::Identity`, the persistent in-process [[map/core/shell-state|`Shell`]]
behind an `IdentityTransport` (plus the session `Scratch`, the re-seed cwd,
and the interrupt target `EvalReach` interrupts through), or `Seat::Wire`, a
`WireTransport` driving a remote engine, one process per session
([[map/core/engine-protocol|engine-protocol]]) — the
canonical run and probe vocabulary either way
([[map/core/engine-protocol|engine-protocol]]) — plus the `inbox`,
the nudge `Registry`, the schedule `ScheduleRegistry`, and every other field
only the attend thread touches, beside the `Arc<Agent>` it embodies
(`.agent`, `pub(crate)` so a caller outside `agent` reaches identity and
config at `.agent.…` directly). Every method that runs the agent takes
`&mut Avatar`.

**An agent is live while its `Avatar` holds that `Arc`.** Nothing
deregisters: `Avatar`'s `Drop` (`agent/build.rs`) clears its own armed
schedules and records the session-ended bookend, and the last strong
reference going with it is what a parent's `children` or the fleet's
`roots`/`names` prune away at their next walk. There is no `Session`, no
`is_root`: every distinction reduces to **position in the tree**, read from
`Agent::parent` together with `interactive` and the agent's own exchange
clock. What every node *shares* for identity resolution and the idle lease
— the by-name door — lives on the thin [[#The Fleet|`Fleet`]],
not on the node; the one [[map/exarch/frontend|`FleetBus`]] and the
transport `Engine` are session-level handles the frontend builds and holds,
not fields of `Fleet` itself. Output caps
are fixed `agent/digest.rs` constants, not per-agent state.

## Status, the exchange clock, and the lock rule

Two per-agent mutexes exist, `Agent::status` and `Agent::children`. **Rule:
hold at most one at a time.** `children` is locked only to push
(`Agent::adopt`) or to snapshot (`Agent::children`, which prunes as it
copies); every walk (`Agent::walk`, `descendant`, cascade, roster) runs
over the snapshot, never under the lock. `status` is a single-writer
register — the avatar writes one field at a time (`set_resting`,
`deposit_reply`, `heard`, `message`, `clear_subtree`) and a reader takes
one snapshot and computes nothing under the lock, so the mutex buys
atomicity and nothing more. `parent` is a plain `Arc`, read lock-free.

**The park verdict reads no fact another thread can change without a
delivery.** `Avatar::park_mode` runs under the consumer's own queue mutex
(`Inbox::next_or_idle`) and before the pop, and each of its inputs is one
of three kinds. The exchange clock is *under that same mutex*: `Queue {
posts, last_exchange }` is what the mutex guards, so `Mailbox::steer` and
`Agent::message` stamp and enqueue in one acquisition (`Mailbox::exchange`),
and `next_or_idle` hands the verdict its reading as the `engaged` argument.
The agent's own `status` — `reply`, and `awaiting`, the set of direct
children spawned (`adopt`) or messaged (`message`) and not yet heard from
(`heard`, at the moment the attend loop takes up the child's
`AgentResult`) — is written only by the consumer's thread, so
`has_busy_children` is *live children ∩ awaiting* and never reads a child's
own `rest`. What remains — a child dying, a schedule firing — happens only
*after* a delivery into this queue, which the pre-pop verdict cannot lose
to. A `Status` writer drops its guard before pushing to any inbox; that is
the whole of the cross-type lock discipline.

**`last_exchange` lives on the inbox, not on `Agent::status`.** It was never
agent state — it is "when did a human or a parent last push into this
mailbox", a fact the `Mailbox` itself witnesses. `Agent::engaged()` reads
it, `Agent::idle()` reads
`mailbox.last_exchange().unwrap_or(started).elapsed()` — the roster's
`idle-s` and the reaper's bound, in one door.

**Up is strong, down is weak.** `Agent::parent` is a strong `Arc`, so
`deposit_reply` and the scope climb (`Agent::descendant`) never dangle: a
parent whose own avatar has gone is still a reachable `Agent` with a
terminated token, not an absence. `Agent::children` holds `Weak`, so there
is no cycle to reason about and a walk prunes what has settled. **Nothing
holds an `Arc<Agent>` to a descendant** — every reaper closure
(`fleet.rs`'s `lease_fire`) and every upward result (`AgentResult`) carries
a name or a `Weak`, never a strong handle down the tree.

The **trunk** is the parent-less node (`parent = None`), built by
`Avatar::root(RootConfig, RootSeat, provider)`: `RootConfig` carries the
prompt, caps, `fuel` (exarch's and synod's launch sites pass `SPAWN_FUEL`),
the IT-set `Egress` (`exarch/src/egress.rs`), and the optional `dial` a wire
trunk reaches its children through ([[#Wire-seat spawn|below]]) — each set
once at launch and inherited verbatim by every fork — while `RootSeat` picks
the seat kind (`Identity` boots its own shell from `scratch`; `Wire` adopts a
built transport whose engine lives elsewhere, and spawns its sub-agents by
dialling back into it).

`Egress` bundles the two things a fleet's outbound network shares across
every fork: `net_policy::NetPolicy` (`exarch/src/net_policy.rs`, the IT-owned
allowlist read from `/etc/exarch/net-policy.ral` or the embedded default —
an exact `hosts` list of lowercase ASCII DNS names plus `search`, with the
retired `read`/`write`, `max-bytes` and `rate-per-minute` keys now hard
errors naming their replacement) and an `AuditLog` (`exarch/src/egress.rs`),
reduced to one `Tunnel` record per attempt: its final vetted address, and on
close the byte count each direction carried — telemetry, not policy. None of
it is a model-facing verb any more — there is no `fetch-url` builtin
([[map/exarch/builtins|builtins]]). The same `Egress` a trunk opens at launch
is what `guest-net::Config::egress` takes: the policy and ledger a synod
session's guest network is gated by are the fleet's own, not a second
copy ([[design/egress|egress]], [[map/synod|synod]]). Host-mode exarch, which
has no guest to police, still keeps `Egress` for one thing: the `search` bit
that clamps the harness `` agents `start [... search: …] `` field
([[map/exarch/builtins|builtins]]).

An `interactive` node
built to converse — the interactive trunk, and every `/branch` tab
([[decisions/260705_branch-minimal|branch-minimal]]) — withholds `reply` and
parks for its human, but both behaviours fall out of construction and position,
never an `is_root` branch:

```
  returns(a)    // fixed at construction: fork true, branch false, trunk ¬interactive
  conversing(a) =  a.interactive ∧ ¬returns(a)
  park_mode(a)  =  Quiesce        if conversing(a) ∧ a.cancel.terminated()   // /close reaped it
                   Held           if conversing(a)                          // immune to cancellation
                   Engaged        if a.parent ∧ returns(a) ∧ (engaged ∨ a.has_reply())  // talked to, or holding a reply; a terminate cause still ends it
                   HeldByChildren if some live child of a is still awaited
                   UntilCancelled if a.schedules.armed()
                   Quiesce        otherwise
```

`returns` (`agent.rs`) is a **construction-fixed field** — one bit read by
`returns()`, parking's conversing predicate, the desk's `reply` refusal
([[map/exarch/builtins|builtins]]), and the prompt's per-agent resolver. The
resolver applies the index from `returns`, `allow_schedule`, and
`spawns = fuel > 0`, so no agent is shown a verb or section the desk would
certainly refuse; the nudge layer, parking, reply availability, and advertised
vocabulary cannot disagree
([[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]).
`park_mode` (`agent/attend.rs`, returning a `ParkMode` of `Held` / `Engaged` /
`HeldByChildren` / `UntilCancelled` / `Quiesce`, `bus/inbox.rs`) is the `should_park`
verdict: a conversing node parks `Held`, immune to cancellation; a returning,
parented agent that is *messageable* — someone has messaged it, or it holds a
deposited reply — parks `Engaged`, the same wait except a terminate-class
cause still ends it, since an exchange is not a conversation — unless its
own cancel token has been terminated (`self.agent.cancel.terminated()`, the
one self-check not answered by a `Weak::upgrade`), which is what `/close`
stamps, since a conversing root has no parent to prune it away and parking
`Held` past that stamp would be a zombie; busy children hold it until they reply or
die; a live self-schedule holds it until cancelled; otherwise it terminates at
quiescence — the one-shot contract a headless trunk satisfies. `--chat` builds the trunk with no system
prompt, no tool at all (`tool_enabled: false`), and no nudge registry — a bare
conversation, the same attend loop.

## The attend loop

Three nested loops, the same for trunk and child alike:

- `attend` — the per-agent lifetime. The trunk publishes its sticky cancel token
  for the OS-signal path (`cancel::publish`, called at each site that launches
  a process trunk — `headless::run`, `headless::converse_settled`, and
  `tui::tui_loop::run` — held for the whole
  attend); a sub-agent publishes nothing, since its token
  is reached through the tree cascade (`Agent::cancel_tree`), not the slot. Each pass pulls the next
  item from this agent's [[map/exarch/frontend|inbox]] via
  `next_or_idle(|| self.park_mode(), …)`, which **re-evaluates the park verdict
  on every `Condvar` wake** — so the idle lease's terminate-cause cancel, or the
  last live child settling, is seen on the very next wake — and hands it to
  `take_up`, the per-item step shared with `attend`'s bounded
  twin `attend_backlog` (converse's per-exchange drain): every item reaching
  here already survived the inbox's own pop-time fence, so what is left is the
  exchange-boundary latch reset, a session command's dispatch to `Control`, the
  `deliberate` call itself, and the nudge reaction. A genuine
  exchange boundary resets the nudge budget and clears the sticky cancel token
  (`cancel::Token::reset`), so a prior exchange's Esc cannot bleed into the next; a
  self-nudge is the same exchange continuing and resets neither. A child's
  `reply` deposits its value on its own `Agent` (`Agent::deposit_reply`, which
  also posts the one-line notice) and the loop parks `Engaged`; only the
  headless root's `reply` ends its loop. At the single exit the loop winds a stranded
  prompt back through `quiesce` so the next `append_user` is always admissible
  ([[invariants/turn-ends-ready|exchange-ends-ready]]); the trunk's `Avatar`
  then drops (a child's drops when its detached thread returns), pruned from
  its parent's `children` and the fleet's doors at their next walk — the
  whole of what deregistration used to require — so the fleet empties as its
  last agent's avatar goes. A panic `take_up` catches around its
  `deliberate` call is a
  *host-side* fault (provider transport, surface decode, render, digest),
  recorded as `AgentOutcome::Failed`; an eval-side panic never unwinds this far —
  the engine's own run door (`Shell::run`) checkpoints `env` / `context`
  at entry, rolls them back, and reports the failed run,
  durability being engine-owned ([[decisions/260612_exarch-panic-recovery|panic-recovery]]).
  The per-call host — `RunHost`, wrapping the desk and applier
  ([[map/exarch/shell-eval|shell-eval]]) — is never installed as shared
  state: it is a plain `Arc` `Avatar::run_shell` builds and holds on its own
  stack, so a panic unwinding through `deliberate` drops it with everything
  else there, with nothing separate to retire.
- `deliberate` — one prompt stepped to quiescence over the agent's *own* provider
  (`self.provider.current()`, read once at the top by `take_up` so a `/model` swap
  lands next item, never mid-deliberation): render the transcript, stream a reply through
  `provider.complete` (one `step`), **admit** then append the assistant message, run the
  resulting tool-call batch (`run_batch`), append their results, optionally append a drained steering prompt,
  repeat until the model emits no tool call. The admission step (`admit_assistant`,
  at the commit boundary) enforces the [[invariants/transcript-admission|transcript-admission invariant]]:
  it repairs a non-object tool-call `fn_arguments` to `{}` (X2) and substitutes a
  stub for an otherwise-empty assistant message (X7), so every committed message
  serialises to a request a strict backend accepts. `StopReason::MaxTokens`
  *with no captured tool call* raises `ProviderError::Truncated` after appending
  the partial, so a `continue` nudge keeps the work as context; *with* captured
  tool calls it dispatches them and continues instead, since returning `Truncated`
  there would strand the protocol in `AwaitingToolResults` and fail the nudge's
  next `append_user` (X6). A hard `MAX_STEPS` ceiling (250) ends a deliberation whose
  model never stops calling tools — the headless/autonomous counterpart to
  interactive Esc — returning `deliberate::Outcome::Capped`. That outcome matches no
  nudge rule, so `attend` treats it as terminal; re-attending would only spend
  the ceiling again.
- `run_batch` — runs one step's tool-call batch in order, each call through
  `invoke`. Every call returns a
  `SessionToolResult` synchronously — and there is only `ral` to call
  ([[map/exarch/tools|tools]]); a spawn verb inside it hands the script a start
  receipt after launching the detached child. Once every requested tool id has
  a result,
  `run_batch` drains this agent's [[map/exarch/frontend|inbox]]'s tool-boundary
  steering. A non-slash steering prompt is appended after the complete
  tool-result batch, and the next step asks the provider with the user's steering
  in context
  ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]]). A
  sub-agent has no human writer, so its inbox holds no steering and this is always
  empty.

  A streamed step closes through one `close_step` door: it seals the answer and
  reasoning choppers, then publishes `Transient::Boundary`, even when the
  provider or cancellation path already carries an error. `abandon_step` uses
  the same ordering best-effort, so a live display edge cannot survive into the
  next step ([[map/exarch/frontend|frontend]]).

Worker-reap and large-binding notices need no drain at `attend`'s top:
core's own engine pushes both as `` `notice `` surface classes at the ready
boundary of the run that produced them
([[map/core/engine-protocol|engine-protocol]]), decoded by
[[map/exarch/shell-eval|shell-eval]]'s `decode_surface` into `Surface::Notice`
and recorded/rendered from there. A reap notice names a worker removed by policy — the lease
chain's idle or backstop bound on a running worker, or the retention sweep
expiring a settled entry's unclaimed result — rather than one an eliminator
observed away. Transcript and TUI only — the rendered one-liner is
[[map/exarch/cards|cards]]'s `reap_card`, the completion card's sibling — never
model-facing, since delivery of a reap to the model itself is deferred.
What `attend`'s
top still runs, each pass its own ready boundary: `check_disk_warn`. (The
protected `services` pin and its reconciler are gone —
[[design/pins|pins]].)

The retention clock itself is core's: the engine ticks the worker registry
once per source dispatch and sweeps it at each settled run's ready
boundary ([[map/core/shell-state|shell-state]]), armed with
[[map/exarch/shell-eval|shell-eval]]'s `SETTLED_WORKER_RETENTION`. The
agent keeps its own mirror of the same drum — `Avatar::ral_epoch`,
incremented once at the top of every `run_shell` call, a failed eval still
a call — which `/resources` reads to render nearest time-to-reap and
`check_disk_warn` reads for its amortisation; the two clocks coincide
one-to-one (`decisions/260629_agent-binding-reaping`). The counter starts
at 0, a fork's child starts its own at 0, and `/clear` does not rewind it.
Retention notices need no plumbing of their own: they ride the same drain
above.

**The binding-lease ledger** is armed by `bootstrap::arm_session_ledgers` —
the one policy site, run by the identity seat's ceremony right after the
session-dir seeding (seeding then arming stay one visible sequence) and by
the wire engine's own boot (`engine_boot_shell`) — with
[[map/exarch/shell-eval|shell-eval]]'s `BINDING_IDLE_CALLS` (256) and
`LARGE_BINDING_BYTES` (1 MiB), beside `arm_worker_retention`. The
large-binding residency nudge rides the pushed `` `notice `` channel above,
and the prune half is engine housekeeping too: idle top-level names fall at
the engine's own ready boundary, announced as a pushed
`` `notice [kind: `prune] `` class the host decodes into the same
recorded `Display::Notice` posture as a reap. The engine's
run-entry checkpoint orders after any prior boundary's prune, so a later
panic rollback can never resurrect a name a pass just pruned.

`/resources` is the probe fold over the same accumulators
([[invariants/probe-convention|probe-convention]]): routed exactly as
`/clear` — an `Item::Command` drained at the exchange boundary, handled by
the TUI's `Control` against the agent the attend loop owns —
`Avatar::resource_rows` surveys what this thread may legally read — the worker
registry's running/settled split with the nearest time-to-reap and the
binding-ledger figures read as *data* through the transport's Enquiry desk
(`probe_workers` and its sibling probes,
[[map/core/engine-protocol|engine-protocol]]), plus inbox depth per
source, the event ledger's logical length and history bytes, log-dir and scratch
disk walked at invocation, and the sub-agent idle lease as two rows (nearest
time-to-reap, and the demote threshold) — and
`emit_resources` posts one `Transient::Resources`
carrying the raw rows beside their already-rendered card together — chrome
only, so unlike a recorded observation there is no later fold to re-render one
from.
A probe fold is an interactive diagnostic, read when it is run: no session
keeps a pressure history, so the figures live only in this emission, never
in `record.jsonl`. The frontend appends the rows for the accumulators *it*
owns (viewports, views, the bus) at render time; neither half reaches
across a thread. Probing never mutates and never renews a lease —
enumeration is not observation — and the fold is never model-facing.

The inbox's per-source depth is a probe figure, not a quota.
`Mailbox`/`Inbox::push` is infallible, one shared rule (`Shared::push`)
split by source: the three idempotent sources (`user`, `schedule`, `nudge`)
coalesce instead of growing the queue — a `ScheduledWakeup` replaces a
still-queued wakeup for the same schedule id (newest wins), consecutive
`UserSteering` pushes merge one per line (never across a slash line, which
would silently change its exchange-boundary classification), and a `Nudge`
replaces any still-queued nudge — newest wins, since a second means a
fresher continuation superseded the first, not that both are owed.
The other four (`AgentResult`, `AgentMessage`, `Command`, `Surface`) simply
queue. A per-source cap of 64 with a 256 total once rejected them, on the
long-session-budgets principle that every accumulator needs a bound; it was
retired because none of them is machine-floodable — a child or worker posts
its result once, a slash command arrives at a human's typing rate, and a
`message` costs its sender a model turn that fuel already bounds — so the
cap could fire only wrongly, and when it did the child parked with its reply
staged but its parent never notified, the very silent loss the cap was meant
to rule out.

The headless-completion gate is gone with `expect_action`: the one role flag
that did not fit the `parent` collapse is dropped, not relocated. What
remains splits into two disciplines, named and owned by `agent/nudge.rs`'s
`Nudges` (`Avatar::nudges`, `None` for a toolless `--chat` trunk): a
**repair** — `Empty`, `Stopped`, `Truncated`, or a returning agent's
`Complete` without `reply` — is event-shaped, exceptional, and spends the
per-exchange repair budget (`BUDGET`, 3); a **standing condition** — the pin
register, the context-pressure gauge — is a fact about the agent's own live
state, **edge-triggered**: told once when it changes, silent while it holds,
budget-free. Both still compose into at most one `EXARCH_REMINDER` message
per completion, self-posted as `Post::Nudge`, committed by `append_user`
inside the same exchange. `react`, the sole post-attempt entry point, gates
everything on `quiet` — no standing reply, no detached shell work, no busy
children, the one condition every nudge kind shares — before it reads the
attempt:

| kind | trigger | budget |
|---|---|---|
| empty-turn repair | quiet ∧ `Ok(Empty)` | spends |
| early-stop repair | quiet ∧ `Ok(Stopped)` | spends |
| truncation repair | quiet ∧ `Err(Truncated)` | spends |
| reply repair | quiet ∧ `Ok(Complete)` ∧ `must_reply` | spends |
| pin reminder | quiet ∧ `Ok(Complete)` ∧ register non-empty ∧ digest changed | free |
| pressure warning | quiet ∧ `Ok(Complete)` ∧ gauge `Over` ∧ excursion untold | free |

Everything else is accepted as-is, unspent, and deliberately so: `Replied` (a
reply is final), `Cancelled` (the human asked), `Capped` (a nudge would only
buy the deliberation another `MAX_STEPS` after it already burned 250 round
trips without quiescing — the wrong channel for a terminal condition, which
already reaches its consumer through `AgentOutcome::Stopped("step cap
reached")`), and every unclassified provider error (the transport's own).

`Nudges` is the one owner of every nudge-relevant latch: the repair budget
`used`, the pin digest last told (`pinned_told`), and whether the live
pressure excursion has been told (`pressure_told`). Nobody else holds
state — `pressure_gauge` is a pure `&self` reading with no latch of its own,
and `take_up` neither latches nor unlatches around it. An edge is **consumed
at decide time, in the same act as the part's emission**, so "told but not
sent" and "sent but not told" have no spelling. `reset()`, called on every
exchange-opening item, clears only the budget — a new exchange is not a new
condition, and clearing the edges there would re-tell an unchanged pin once
per delivered item. Every path that discards a decided-but-uncommitted nudge
instead rebuilds `Nudges` whole: `/clear` (`Inbox::clear` sweeps the queue,
`Avatar::clear` rebirths) and `/rewind` (`drop_nudges` sheds the queued
nudge, `rewind` rebirths) — the rebirth also covers a `/rewind` typed mid-
deliberation, whose `Barrier` queues ahead of the nudge `take_up` posts
after it: the nudge is decided (its edges already consumed) and then shed,
so without the rebirth the telling would be recorded but never committed.

Edge-triggering the pin reminder closes a livelock: a stationary register can
now produce at most one nudge, where a bare per-completion reminder let a
`Complete`, its own reminder, and the next `Complete` cycle forever while the
register sat unchanged — the no-pins `set-goal` advertisement that used to
steer an agent straight into that state is deleted outright with it. A
standing condition staying *event-shaped* on the wire looks like it should
strain the fold law
([[decisions/260812_context-is-a-projection|context-is-a-projection]]'s "one
state, one fold") — a condition that *holds* seems like it should not be
re-stated per turn — but it does not: the transcript is already persistent,
so a committed turn stands in every later render until an edit folds it, and
re-stating the register on every turn was never persistence, only
redundancy. What must be recorded is the level's *transitions*, and an edge —
"the register changed", "pressure crossed the line" — is genuinely
event-shaped, so recording it as a committed user turn is honest, not a
workaround. The alternatives are strictly worse against the law: a
render-time seam re-injecting live state would break `fold(log) == memo`,
since the model's view would stop being reproducible from the record; a
dedicated "standing condition" record class still has to enter the fold to
reach the model, so it is a user turn with a fancier name plus a new record
variant, fold arm, and admission rule — machinery for no semantic gain. A
compaction may fold a telling into the digest without harm either way: both
conditions self-heal regardless — pressure re-fires per excursion by
construction, the pin reminder re-fires on the next register change, and the
model can always `pin-list`.

A `--chat` trunk holds **no `Nudges`** (`nudges: Option<nudge::Nudges>`,
`None` when the tool is withheld): every nudge steers the model toward a
tool it does not have, so no rule runs, no reminder fires, and nothing
synthetic ever joins the conversation. Its provider errors still reach the
human, since that report is the attend loop's own step — `take_up` emits
`Forensic::ProviderError` for whatever error the attempt carries before it
ever asks for a nudge, so `react` decides and nothing more.

## The Fleet

`fleet.rs`'s `Fleet` is `{ names: Mutex<HashMap<String, Weak<Agent>>>, roots:
Mutex<Vec<Weak<Agent>>>, lease: Duration }` — two `Weak` doors and the
idle-lease bound they share, fixed at construction, nothing else. It is not the tree: the tree is
`Agent::parent`/`Agent::children`, and every walk — the roster, the cancel
cascade, the scope check — runs there. `names` is the by-name door a spawn
claims identity at and `` agents `message ``/`` `cancel ``/`` `read ``
resolve through (`Fleet::resolve`); `roots` holds the trunk and every
`/branch` — a root reports to nobody, so a walk from `roots`
(`nearest_reap`) is how the idle-lease scan reaches every live agent in the
run.  The frontend arrives through no fleet door at all: each tab was handed
its agent's `Weak` on the birth notice
([[map/exarch/frontend|frontend]]). Both are `Weak`, and a lookup prunes what has
settled as it walks. `FleetBus` and the `Arc<provider::Engine>` are not
fields of `Fleet` — the frontend builds its own (`FleetBus::session`) over
the trunk's inbox and holds the engine handle separately; what every node
shares through `Fleet` is only identity resolution and the lease.

- **One door: `Fleet::enrol`.** Construction is the only place an agent
  joins the fleet, and every refusal (`Unborn::SessionDead` — the parent
  raced a cancel; `NameTaken`; `NameMalformed`, checked here too since a wire
  peer is not trusted to have used `check_name`) is decided under the one
  lock a racing spawn's own claim takes. A refused agent never appears in
  the fleet, however briefly: the name is claimed, then — only then — the
  agent is adopted into its parent's `children` or pushed onto `roots`, and
  a parented birth arms its lease (`arm_lease`).
- **The fleet is alive while `roots` prunes to empty** — the literal "dies
  when no active agents remain" (`Fleet::is_empty`). An agent leaves by its
  avatar dropping (quiescence, cancel, a non-reply finish); a conversing
  node stays until `/quit` (or `/close`) because it parks; a headless trunk
  leaves at quiescence; a replied sub-agent parks under its idle lease and
  leaves when it is reaped. There is no human-less daemon: nothing lingers
  without a present human, running work, a bounded lease, or a bounded
  self-schedule.
- **The idle lease is dynamic; focus is not.** A leased child — every
  agent `Fleet::enrol` gave a parent, i.e. every returning sub-agent; the
  lease is a consequence of having a reporting parent and of nothing else,
  no caller chooses it — is reaped once its idle span (`Agent::idle`,
  measured off the *inbox's* last-exchange clock, seeded at birth) exceeds
  the fleet's bound: the reaper (`fleet.rs`'s `lease_fire`, on the process
  reaper daemon thread) re-arms itself for the remaining margin on every
  fire that finds the agent's `Weak` still upgrading and under bound, and
  cancels the subtree with `CancelCause::Deadline`
  (`Agent::cancel_tree`) once it is not. The one thing that renews the
  clock is a delivered message — a human's (`Mailbox::steer`) or the
  parent's (`Agent::message`); nothing else does — not the TUI's `TAB`
  cursor, a plain, presentation-only `AgentId` local to the frontend
  (`tui::tabs::Tabs::focus`) that neither the fleet nor `park_mode` ever
  reads, not a `/resources` probe. A returning agent's `reply` cancels its
  proper descendants and
  parks it, regardless of which tab the human's cursor sits on.

## Cancellation cascades the subtree, across both layers

The single cascade serves the deliberate teardowns — `` agents `cancel ``, a
returning agent's `reply`, and the `/clear` / `/close` subtree reaps — and it
runs over the agent tree itself, not a registry. `Agent::name` is its
identity — well-formed and unique among live agents, both enforced at
`Fleet::enrol` (`Unborn::NameMalformed`/`NameTaken`; the trunk holds
`TRUNK_NAME`, `agent/build.rs`), which is where a name *becomes* identity and
so the one place a wire peer cannot go around. The desk refuses both a beat
earlier, before it forks a log or dials anything, but only `enrol` is
authoritative. A name is also the handle `` agents `message ``/`` `cancel ``
resolve descendants by, through `Fleet::resolve` and then the scope climb
(`Agent::descendant`). Every `Agent` carries a strong `parent`, so the tree
is the spawn tree directly: `Agent::cancel_tree` cancels an agent and its
whole subtree, `Agent::cancel_descendants` abandons a returning agent's
children, and `Agent::clear_subtree` reaps a subtree and forgets what it
was `awaiting`. The fence a late result or deferred surface batch must
survive is no longer here: `Avatar::clear` drains this agent's own inbox,
and that drain is what bumps the clear-epoch a `bus::Stamp`-borne message
is judged against at its own pop. The epoch is per inbox, not per fleet,
and the envelope a worker carries is its *reader's* by construction — a
`/clear` in one tab must not throw away work another tab is still waiting
on. Each cancelled node is
stopped **across both layers**:
its cooperative `Token` (read by `deliberate` between steps and raced by
the provider's mid-stream cancel) *and* the eval layer through its own
`reach: EvalReach` (fixed at construction — every agent carries one, the
trunk included) —
`EvalReach::Identity` holds the session's durable
root (`Shell::cancel_handle`) for `terminate` and, for `interrupt`, the cell its
transport publishes each dispatch's scope into as that dispatch is minted —
ahead of the engine lock, so an interrupt racing a dispatch still waiting on the
lock reaches the run about to be born rather than the one just ended
([[internals/cancellation|cancellation]]) — while `EvalReach::Wire`'s only
host-reachable primitive is
`Control::Cancel` on the in-flight dispatch, so both motions resolve to it
— and a `ral` eval already in flight
unwinds at the evaluator's poll points instead of grinding to its
`timeout_secs` wall. The trunk's reach is *interrupt-only* —
`EvalReach::interrupt_only` clears its `eval_root` to `None` at construction,
so a `terminate` there degrades to the `Token` alone: its session outlives any
cancel, and a captured root would both permanently poison it and go stale at
the next `/clear`, which rebuilds the trunk's shell in place while an agent's
reach is fixed once, at birth. Esc also reaches the trunk's exchange through the
ambient foreground cause, which only the trunk's session is minted facing
([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]],
[[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]],
[[internals/cancellation|cancellation]]). `Esc` / Ctrl-C, by contrast, are a
**per-tab exchange interrupt**, not a cascade: they stop only the *focused* agent's
current exchange (`Agent::interrupt`, reached through the focused tab's own
`Weak`, plus `cancel::raise_interrupt`
on the trunk), leaving its descendants running
([[decisions/260705_cancel-per-tab|cancel-per-tab]]); the focused agent's
sticky token is cleared at each exchange boundary (`Token::reset`).

Cancelling `eval_root` already reaches a cancelled node's own detached `ral`
workers with no edge of its own: a worker's cancel scope is a child of its
shell's durable root, and every `CancelScope::is_cancelled` walks its
ancestors. What the cascade does *not* reach is a node that ends without ever
being cancelled — the ordinary `reply`/settle path, or the trunk's own
end-of-`attend` exit, where the avatar simply drops. `Avatar`'s `Drop`
(`agent/build.rs`) closes that gap in one place: it clears its own armed
schedules unconditionally and records the session-ended bookend — the
underlying seat's own teardown cancels the workers registered on its shell —
the same law `clear` already applies explicitly below, so a settled-but-never-cancelled
agent leaks neither
([[design/residency|residency]], [[decisions/260705_session-ledger|session-ledger]]).

## The provider is per-agent and hot-swappable

`ProviderHandle` is owned by the `Agent`, not threaded through `attend`'s
parameters. `take_up` reads `self.provider.current()` once per item. `/model` swaps
the **focused** agent's handle directly on the UI thread (resolved through
the fleet by id), so a
swap on one agent never disturbs another. `fork` seeds the child's own handle
from the parent's current provider (`ProviderHandle::new(self.provider.current())`),
so the child inherits the model in force at spawn and may diverge afterward.

## Lifecycle: clear, compact, resume, fork

`clear` rebuilds the focused agent without carrying cancellation residue forward:
it drops the waiting inbox before the reboot, so a prompt typed during that
reboot belongs to the new context, then re-runs the seat's ceremony
(`Seat::clear`) — the identity seat reboots a
fresh shell from `boot_root_shell` (`agent/seat.rs`, the cwd- and
scratch-seeding wrapper over `bootstrap::boot_shell`) onto the *same*
interrupt target; a wire session instead clears by killing its engine
process and booting a fresh one from the same recipe, so no caller routes
`/clear` to that seat. The identity seat rotates `record.jsonl` to the
first-free `.n`, then starts a fresh record ledger; it never truncates the
old record. The record rename is the rotation commit point. The rotation
swaps the *file* behind the seam, never the seam: the `Emitter` and its
attached bus sink are the session's, so every clone already handed out — the
surface buffer's, the frontend's coupled channel — writes into the new
segment and keeps publishing. Swapping the `Emitter` instead stranded them
on the rotated-away file, and the frontend, whose clear-drain gate opens on
the `Transient::Cleared` that seam publishes, stayed dark for the whole first
exchange of the cleared session. Clear also
clears the schedule registry and cascades cancel to its subtree. Replacing the
transport drops the outgoing shell, whose `LocalState` teardown cancels
every worker still registered on it — explicit destruction outranks every
lease, the durable class included, with no host call site to forget
([[map/core/shell-state|shell-state]]). A worker settling after the cancel still tries to flush
its deferred `done` batch through the boundary it captured before the clear;
the same pop-time epoch fence ([[map/exarch/shell-eval|shell-eval]]) that
already drops a stale agent result drops that flush too, so no pre-clear
worker output survives into the rebuilt context. It is the focused agent's,
not a fleet-wide reset.

`--resume` is a trunk-only lifecycle: it validates and replays session 0's
`record.jsonl`, quarantining only an unterminated crash fragment, then reopens
the file for append. The live model and provider are selected again, while the
shell is fresh and receives a note describing the bindings, workers, cwd,
scratch, pins, and schedules that were not durable. Wire seats and child logs
are refused rather than half-resumed.

`--no-logs` chooses the mirror-only path at birth: no durable event ledger or
transcript, and no run lock, is created. The in-memory model view and live bus
still operate, children inherit the choice, and there is consequently nothing
for `--resume` to reopen.

`compact` runs `provider.summarize` over the closed prefix when context pressure
crosses the window's reserve (`digest.rs`'s `compaction_due` — used tokens
into the top 15% of a known window; `COMPACT_THRESHOLD`, 500 KiB of serialised
history, is the fallback when the window is unknown) and `AgentLog::can_compact`
holds (no pending tool results). It is called at the **top of `deliberate`**,
where the agent is `ReadyForUser` ([[invariants/turn-ends-ready|exchange-ends-ready]])
and the gate actually holds — every provider round-trip (`step`) passes through
here, so long autonomous and headless sessions stay bounded without an
interactive `/compact`. An exchange-boundary Esc bails before the summarize
request ([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).

A successful compaction records `ContextEdited { op: Fold, by: Harness }` with
the digest and cut. The same fold step removes the prefix spans from the model
view and evicts their resident events, retaining only their byte ranges in
`record.jsonl`; this is residency following the view, not a second drain rule.
There is no `Compacted` state or archival cut to infer. A failed attempt records
nothing and sheds nothing. The durable log is appended to, never rewritten.

Auto-compaction is one authority over the same `Fold`/`Drop` edit
([[decisions/260812_context-is-a-projection|context-is-a-projection]]). The
model reaches the other two: `context-read` and `context-drop` let it survey
and shed closed exchanges of its own choosing, each recording `ContextEdited`
with `EditAuthority::Model` rather than `Harness`. The user's own hand is
`/rewind <exchange>`, which desugars to the same `Drop` at
`EditAuthority::User`, sheds queued self-nudges, and rebuilds the nudge state;
`/context` surveys the transcript without editing it, the read-only sibling
`ReplControl::command` serves alongside `/clear`, `/compact`, `/branch`, and
`/quit`.

`Avatar::check_disk_warn` is the disk half of the same ADR ("Disk: report
and warn only") — report-and-warn only, never rotation or deletion.
Unconfigured (`config::disk_warn_bytes` absent, the default) it is a no-op
by construction: no walk, no cost, ever. Configured, it rides the same
`ral_epoch` the settled-worker and binding-lease sweeps already read,
amortized to once every `DISK_WARN_CHECK_INTERVAL` (32) calls, at the same
ready boundary `attend`'s loop walks each pass.
Crossing the ceiling (session log dir + `EXARCH_SCRATCH`, summed via the
existing `resources::dir_size`) emits one `Forensic::SystemNote`, latched until
a later check finds the total back under — one warning per excursion, not
one per boundary.

A fork builds the child `Avatar` (and, inside it, the child `Agent`) for
[[design/agents|sub-agent spawning]] through
`Shell::fork_session` ([[map/core/shell-state|the flow matrix]]) rather than
hand-copying fields after a bare `Shell::new`. It takes the child's
`Capabilities` **as an argument**, so the spawn site owns the authority decision
(the parent's verbatim, or `parent ⊓ base` via [[map/exarch/policy|`policy::narrow`]]).
The child sets `parent: Some(self.agent.clone())` — the strong tree edge that
routes its result and drives the subtree cascade — and enrols itself in the
fleet (`Fleet::enrol`), joining the shared `Arc<Fleet>` every node holds.
It snapshots the **serialisable fragment** of the parent's lexical scope
(prelude, agent library, every accumulated binding that has a wire form —
`Shell::fork_scrubbed` drops `Value::Handle` bindings, and both spawn arms
fork through it, so an identity fork and a wire hatch's `EngineSeed` carry
the same scope, [[design/agents|agents]]), its dynamic context (cwd, env,
grants, handlers),
and the installed builtin table, and starts fresh in everything else — fresh
control counters and a freshly-defaulted `SessionState`, so it holds **no
terminal authority** (`TerminalAccess::Denied`, no lease — a sub-agent is not the
foreground agent and can never seize the controlling terminal the TUI owns).
There is no flow-back: the child's `cd`, env, and new bindings die with it. An
agent with fuel left may spawn, and each fork hands the child one less unit of
`fuel` than the parent holds (`SPAWN_FUEL = 3` at the trunk; the parent's own
fuel is never debited, so fuel bounds depth, not fan-out). At `fuel == 0` the
prompt drops the spawn family — `agents` — and the desk refuses
`` agents `start `` with the exhaustion text; the desk remains the runtime wall
([[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]]) — so a delegation
chain bottoms out by refusal a fixed number of generations down. The fork
mirrors on the bus as `Transient::Born` / `Transient::Died` regardless of
remaining fuel.

`Avatar::fork_with(caps, returns, name)` is the shared fork core — a
returning child passes `true`; `Avatar::branch` is
`fork_with(self.agent.caps.clone(), false, name)` plus `inherit_context`,
minting a *conversing* peer tab with the parent's verbatim
authority ([[decisions/260705_branch-minimal|branch-minimal]]) — `returns:
false` means `parent` comes back `None`, so a branch is a root exactly as
the trunk is. A builtin
spawn takes the decomposed path instead: the `` `start `` tag's body leaves
the fork where this run's `Fork` door says, and the desk's `` agents `start ``
arm collects it and calls `Avatar::assemble` at one less unit of fuel
([[map/exarch/builtins|builtins]]).

Prompt resolution is shared across the root, identity-fork, and wire-child
paths. Each keeps the unresolved base and applies its own `returns`,
`allow_schedule`, and child-fuel bits; the resolver appends `Agents`
iff fuel remains and `Agent` iff the child returns. The child's log bookend
records that fully resolved prompt length, including the filtered index and
late sections, so a child never inherits an already-appended `Agent` section.

### Wire-seat spawn

**Both seats spawn in one exchange of the same `` agents `start ``
vocabulary; they differ only in where the fork waits.** The arm is chosen on a
stated fact — `HostServices::kind`, a `SeatKind::{Identity { scratch }, Wire}`
read off the seat when the desk's capture is built, once per `ral` call
(`agent/shell.rs`) — one encoding, so an identity seat without a scratch is
unrepresentable. What varies is the `fork` tag the
*engine* mints beside the model's own spec record, since the reentrancy law
bars a desk handler from holding the `&mut Shell` a fork needs:

- **Identity.** The run's `Fork` door is `Fork::Park(nursery)`, so the
  builtin body calls `Shell::fork_into_nursery` and names the slot as
  `` `parked <id> ``. The desk's arm adopts by id, in the same process.
- **Wire.** A guest engine's runs carry `Fork::Listen` (`core/src/engine.rs`),
  so the builtin body mints a `u64` token from OS randomness and calls
  `ral_core::hatch::listen_for_hatch` (`core/src/hatch.rs`) with a listening
  descriptor `shell_eval/builtins/guest_port.rs` has already bound — the one
  `AF_VSOCK` endpoint exarch opens itself, and Linux-only because a guest port
  means nothing outside a VM, while core's half is plain Unix plumbing tested
  over `UnixListener` pairs on the production path. The scrubbed fork is
  packed into an `EngineSeed` on the caller's own thread — a `Shell` never
  leaves it — and a thread is left on the socket. The enquiry names
  `` `listening [port, token] ``.

The desk's wire arm dials that port through its **`Dial`** capability
(`exarch/src/agent/dial.rs` — `vm_manager`-free by construction, a capability
object `RootConfig` carries, `None` on every identity trunk), writes the eight
token bytes little-endian, and blocks on **one acknowledgement byte**
(`ral_core::protocol::HATCH_ACK`) under the transport's own deadline. The
listener thread accepts and compares the eight bytes, polling the wake pipe
before every partial read — a dial that does not know them is dropped and the
accept loop resumes, while one that sends only a prefix cannot pin shutdown.
Losing the token race costs a stranger nothing but its connection; stalling
mid-token costs the winner its spawn, which the decision records rather than
defends against. `hatch_over` spawns `current_exe --engine` with the connection
on fd 3 and the seed's fd named by `RAL_ENGINE_SEED_FD`; the child reads the
framed seed before waiting for `Attach`, and the parent writes it after
`spawn()`, so even a seed larger than the channel buffer makes progress. It
writes the ack **only once `spawn()` has returned and the seed has crossed**. An
ack therefore means the child already exists and holds its seed. The desk then
adopts the
stream as `Seat::Wire` and hands the child to the same `spawn_async` an
identity fork reaches, at `fuel = parent - 1`.

One exchange, one token, one thread: the whole of failure is local. A refused
enquiry never dialled, so the builtin wakes its listener through a pipe and
joins it, raising the thread's own reason over the host's when it has one,
since it was nearer the failure. Past the ack a refusal simply drops the
stream — the child reads EOF on fd 3, and core's `HATCHED` table, swept at the
next hatch and again at engine teardown, reaps whatever has since exited. That
sweep is `waitpid` and nothing lighter: a hatched child closes its seed channel
as soon as it has hydrated, so silence there means *started*, never stopped. A wire
trunk with fuel > 0 and no dialler is a construction error, refused at
`Avatar::root` with a sentence rather than discovered by a model calling
`agent`; off Linux the wire arm refuses in one sentence too.
The enquiring builtin cannot tell which arm served it — both answer the same
roster `` agents `` gives every tag. See [[design/agents|agents]] for the
seed's isolation law and [[map/synod|synod]] for `MachineDial`, the `Dial`
over `Machine::connect_guest`. The `` `mnemon `` memory mode
additionally forks
the parent's `AgentLog` and imports its model-visible context before assembly
([[decisions/260702_subagent-memory-modes|subagent-memory-modes]]); `AgentLog`
drops a pending unanswered assistant tool-call frame when the parent is
mid-dispatch, so the child inherits a request context rather than a dangling
provider protocol. Both memory modes seed the launch prompt through the
child's inbox, so the prompt enters through the same item path.

Routing the fork through core matters because the builtin table is the easiest
thing to drop. The exarch host builtins — `view-text`/`view-hash`, `grep-files`,
`edit-hash`, `edit-replace`, `explore-dir`, `fff`, the skill loaders
([[map/exarch/builtins|builtins]]) — live in the agent's own dispatch table
rather than in the environment, and the
`view-text-around` helper in `agent.ral` calls `view-text`. A fork that copied
only the scope and the context would leave the child's `view-text-around`
resolving to nothing and falling through to a failed PATH lookup.
`fork_session` snapshots scope, context, *and* the builtin table as part of the
flow matrix, so the decision lives in one place and the table cannot be
silently severed at this call site.

`digest.rs` holds `clip` and the fixed per-section byte caps for what the
*model* sees in history: each tool-result section has its own cap
(`VALUE_CAP` 20 KiB, `STDOUT_CAP`/`STDERR_CAP` 10 KiB), alongside separate caps
for opaque error blobs (`OPAQUE_CAP`) and the history-compaction threshold; a
child's reply is not clipped, since it reaches the parent as a value through
`` agents `read `` rather than as text. An oversize section keeps a head+tail digest
and elides the middle, with a banner nudging the model to scope the query at
its source and re-read in slices — the same rendering the transcript records,
so the user never sees more of a result than the model does. A structured
`VALUE` now arrives pre-budgeted — the printer spends its own byte budget inside
the value, cutting where a container can still name what it dropped — so `clip`
is a backstop there, while stdout, stderr, and raw payload strings still meet it
head-on. `run_shell` here threads to [[map/exarch/shell-eval|shell-eval]].

## See also

[[design/agents|agents]] (the role model these nodes realise — one `parent`
predicate, the conversing trunk vs returning agents),
[[map/exarch/tools|tools]] (the one `ral` tool the
provider sees) and [[map/exarch/builtins|builtins]] (the harness verbs the
desk answers), [[map/exarch/frontend|frontend]] (the bus, the inbox, the
fleet doors, and the two frontends), [[map/exarch/provider|provider]],
[[map/exarch/policy|policy]], [[map/exarch|exarch]],
[[design/residency|residency]] (the resident ledger this cascade and the
worker/schedule teardown edge are chapters of),
[[map/core/engine-protocol|engine-protocol]] (why a wire
seat is one engine process, one connection),
[[decisions/260827_agent-and-avatar|agent-and-avatar]] (why `Agent` and
`Avatar` split this way, what the three-copy shape it replaced deleted),
[[map/synod|synod]] (`MachineDial`, and synod's own helper surface built over
the wire-seat spawn above).
