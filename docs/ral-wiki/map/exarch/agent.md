---
generated_at_commit: 11950445365e2a5381b0bc988f6b6142ae16bb52
generated_at_date: 2026-10-19
covers_paths: [exarch/src/agent.rs, exarch/src/latch.rs, exarch/src/agent/, exarch/src/signals.rs, exarch/src/cancel.rs, exarch/src/schedule.rs, exarch/src/enquiry.rs, exarch/src/prompt.rs, exarch/src/config.rs, exarch/src/net_policy.rs, exarch/src/net_policy/, exarch/src/egress.rs]
---

# Map: exarch / agent

`agent.rs` splits the node every fleet member is into two types along who
may touch what ([[decisions/260827_agent-and-avatar|agent-and-avatar]]).

**`Agent` is the public half**, held behind an `Arc` the fleet shares:
identity (`id`, `name`, `log_dir`), the two cancel layers (`token:
cancel::Token` and the eval-layer `reach: InterruptTarget`, both in the crate-level `cancel.rs`), the sender half
of its own `mailbox`, an owned hot-swappable `provider: ProviderHandle`, the
per-node config fixed at birth (`caps`, `fuel`, `role`, `search`, the
resolved `system` prompt), the single-writer `status: Mutex<Status>` — `{
rest, reply, replied_at, awaiting }` — and a weak `children: Mutex<Vec<Weak<Agent>>>`.
**`role: Role`** says whom the agent answers: `Converses` (an attended or
embedded root, rooting its own tree), `Headless` (the
headless root, replying once to the process's caller), or `Child { parent,
consumer }` — the strong parent edge and the parent's `Stamp`, minted at
birth by `Role::child`. A parented agent that converses is unrepresentable.  Its fields are plain
`pub(crate)`: a record, not an interface, read directly by the modules that
run it.  What every node of a run shares — the host's settings and the
services it opened — is not on the node at all but on the fleet's
[[#The Fleet|`Launch`]], held once.  There is one fence, the
inbox's own clear-epoch
([[decisions/260827_agent-and-avatar|agent-and-avatar]]), and it is carried
only inside `bus::Stamp` — the addressed envelope `Mailbox::stamp` mints,
pairing the destination mailbox with its epoch at composition — so a
message that cannot judge its own staleness (`bus::Stamped`) is unsendable
with a stamp forgotten, skewed to another inbox, or refreshed at push time;
the destination judges it at its own pop.

**`Avatar` is the private half**: the canonical
[[internals/session-record|model projection]] (`AgentLog`, `record/session.rs`), a **seat**
(`agent/seat.rs`) carrying the transport this run drives through —
`Seat::Identity`, an `IdentityTransport` booted through the installer's two
halves (`boot::engine_runtime` and `boot::engine_root`, in the table
`exarch::INSTALLERS` both carriers share) — a root keeps the `Attach` it was born from and the session `Scratch`
that `Attach` names, an adopted fork keeps neither — or `Seat::Wire`, a
`WireTransport` driving a remote engine, one process per session, keeping only
the `EngineSpec` it was attached with
([[map/core/engine-protocol|engine-protocol]]) — the
canonical run and probe vocabulary either way
([[map/core/engine-protocol|engine-protocol]]) — plus the `inbox`, the
`reply: ReplyCell` (a `SharedCell`, borrowed rather than locked, as `LogCell` is for the `AgentLog`: one thread, so a failed borrow is reentrancy and panics) the desk's `reply` handler stages into, the `readings`
(the token `Measure`, the `Gauges`, the `Nudges` — everything this avatar has
measured and told about one context, reborn whole with it), and the
`Arc<Agent>` it embodies (`.agent`,
`pub(crate)` so a caller outside `agent` reaches identity and config at
`.agent.…` directly). Every method that runs the agent takes `&mut Avatar`.
`agent` is the last field on purpose: the fleet's `Weak` must outlive the
seat's teardown and the log's `SessionEnded`, since a root's death is what
ends the process.

**An agent is live while its `Avatar` holds that `Arc`.** Nothing
deregisters: `Avatar`'s `Drop` (`agent/build.rs`) clears its own armed
schedules and records the session-ended bookend, and the last strong
reference going with it is what a parent's `children` or the fleet's
`roots`/`names` prune away at their next walk. There is no `Session`, no
`is_root`: every distinction reduces to **position in the tree**, read from
the agent's `Role`, the launch's `Trunk`, and the agent's own exchange
clock. What every node *shares* — the `Launch`, the
by-name index, the idle lease — lives on the [[#The Fleet|`Fleet`]], not on
the node; the one [[map/exarch/frontend|`FleetBus`]] and the transport
`Engine` are session-level handles the frontend builds and holds, not fields
of `Fleet` itself. Output caps are fixed `agent/digest.rs` constants, and the
eviction thresholds `agent/gauge.rs`'s, not per-agent state.

## Status, the exchange clock, and the lock rule

An **exchange** on this page is a human round — what the fleet's clock counts
and what a cancellation interrupts. It is never a unit of the context, which
has only turns, each with a role
([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]).

Two per-agent mutexes exist, `Agent::status` and `Agent::children`. **Rule:
hold at most one at a time.** `children` is locked only to push
(`Agent::adopt`) or to snapshot (`Agent::children`, which prunes as it
copies); every walk (`Agent::walk`, `descendant`, cascade, roster) runs
over the snapshot, never under the lock. `status` is a single-writer
register — the avatar writes one field at a time (`set_resting`,
`deposit_reply`, `heard`, `message`, `forget`) and a reader takes
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
it, and `Agent::idle()` reads
`mailbox.last_exchange().unwrap_or(started).elapsed()` for the TUI's idle age.
The roster's `idle-s` reads `Status::rest`; the reply lease reads
`Status::replied_at`. These clocks have different origins.

**Up is strong, down is weak.** `Role::Child`'s parent is a strong `Arc`, so
`deposit_reply` and the scope climb (`Agent::descendant`) never dangle: a
parent whose own avatar has gone is still a reachable `Agent` with a
terminated token, not an absence. `Agent::children` holds `Weak`, so there
is no cycle to reason about and a walk prunes what has settled. **Nothing
holds an `Arc<Agent>` to a descendant** — every reaper closure
(`agent/fleet.rs`'s `lease_fire`) and every upward result (`AgentResult`) carries
a name or a `Weak`, never a strong handle down the tree.

A **root** is a parent-less node (`parent = None`), and a fleet holds any
number: each is a session (design: [[design/agents|agents]]). A run starts in
one of two ways. `Avatar::root(RootConfig, RootSeat, provider)` founds the
fleet and hatches `main` (`TRUNK_NAME`) over a fresh `AgentLog::root`;
`Avatar::resume(RootConfig, RootSeat, provider)` lists the run's live roots
(`AgentLog::live_roots`: every `sessions/<n>/record.jsonl` that opens with a
root's `SessionStarted { parent: None, .. }` and holds no `SessionClosed`, in
id order, with its recorded name; a record that cannot be read is an error
naming its session, not a skipped root), founds the fleet seated for the first,
and hatches each over its record replayed (`AgentLog::resume(sessions_root, id)`; the name is `live_roots`' alone),
the rest seated through the
launch's `genesis`. It hands back one `Resumed { turn, bytes }` (`record/session.rs`)
per root, which the TUI's tab notes after mirroring the log.

The two halves are `Fleet::found(cfg, root_seat, installers, lease)`
and `Avatar::hatch(fleet, claim, seat, provider, open)`. `found` seats the first
engine (an identity seat through `Genesis::seat`; a wire seat is adopted as
built), reads the builtin index and `$EXARCH_SCRATCH` off it, substitutes the
scratch placeholder into the prompt template, and settles the `Launch`; it
refuses a fuelled wire root with no `factory`. `hatch` makes one root of that
fleet under the name its `Claim` holds (each caller claims first, then seats, then hatches, so a refused name boots no engine), resolves the prompt
(`Launch::prompt_for`), opens the log through `open` (which takes the prompt
as `system: &str` — the log records it as `Protocol::System` — and may hand a
fact of the opening back beside it, as `resume` does its `Resumed`), and `assemble`s it
under the launch's `Trunk` role, `caps`, `fuel` and `search`. The recorded
account is derived there, as `Avatar::child_of` derives it. `RootConfig` carries
the prompt, caps, `fuel` (exarch's and synod's launch sites pass `SPAWN_FUEL`),
how roots are driven (`Trunk::{Headless, Attended, Embedded}`), the `Toolset`
requests advertise (empty for `--chat`), the `Egress`
(`exarch/src/egress.rs`) whose `search` verdict is read once, and the optional
`factory` a wire root makes its children's engines with
([[#Wire-seat spawn|below]]) — all of it ends up on the fleet's `Launch`, held
once. `RootSeat` picks the seat kind: `Identity` states the cwd, terminal
capabilities and the run's `scratch`, and becomes the launch's `Genesis`; `Wire`
adopts a built transport whose engine lives elsewhere, and leaves `genesis`
`None`. The run's advisory lock is the launcher's to hold, never the node's.

`Egress` bundles the two things a fleet's outbound network shares across
every fork: `net_policy::NetPolicy` (`exarch/src/net_policy.rs`, the
allowlist read from `/etc/exarch/net-policy.ral` or the embedded default —
an exact `hosts` list of lowercase ASCII DNS names plus `search`, with
`read`/`write`, `max-bytes` and `rate-per-minute` refused as hard errors
naming their replacement) and an `AuditLog` (`exarch/src/egress.rs`),
reduced to one `Tunnel` record per attempt: its final vetted address, and on
close the byte count each direction carried — telemetry, not policy. None of
it is a model-facing verb — there is no `fetch-url` builtin
([[map/exarch/builtins|builtins]]). The same `Egress` a trunk opens at launch
is what `guest-net::Config::egress` takes: the policy and ledger a synod
session's guest network is gated by are the fleet's own, not a second
copy ([[design/egress|egress]], [[map/synod|synod]]). Host-mode exarch, which
has no guest to police, still keeps `Egress` for one thing: the `search` bit,
which every fork inherits verbatim. `provider::wire::tool_defs` attaches the
hosted search tool per request, only where the adapter has one, so a `/model`
swap or a `role` child provider never leaves a stale verdict behind.

A node built to converse — every attended or embedded root — withholds
`reply`; whether it also parks for a human is the launch's say, not the
node's. Two facts: the `Role` is per node, fixed at birth; `attended` is per run, fixed at launch — a human at the inbox (the
TUI), or none (synod drives one exchange at a time; headless seeds once).
Both behaviours fall out of construction and position, never an `is_root`
branch:

```
  role(a)         // fixed at birth: fork Child, root by how the run is driven (Headless or Converses)
  attended        // fixed at launch, on the fleet: Trunk::Attended alone
  park_mode(a)  =  Quiesce        if a.seat severed
                   Quiesce        if role(a) = Converses ∧ attended ∧ a.token.terminated()   // a shutdown signal reached it
                   Held           if role(a) = Converses ∧ attended                          // immune to cancellation
                   Engaged        if role(a) = Child ∧ (engaged ∨ a.has_reply())             // talked to, or holding a reply; a terminate cause still ends it
                   HeldByChildren if some live child of a is still awaited
                   UntilCancelled if a.schedules.armed()
                   Quiesce        otherwise
```

`role` (`agent.rs`) is a **construction-fixed field** — one value read by
parking, the desk's `reply` refusal ([[map/exarch/builtins|builtins]]), and
the per-agent resolver (`agent/index.rs`: `BuiltinIndex`, `Grants`, the verb
families and their sections; `prompt.rs` keeps only the shared base and its
placeholders, so `prompt` never depends on `agent`). The resolver applies the
index from the `Role`, the launch's `allow_schedule`, and
`spawns = fuel > 0`, so no
agent is shown a verb or section the desk would certainly refuse; the nudge
layer, parking, reply availability, and advertised vocabulary cannot disagree
([[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]).
`park_mode` (`agent/attend.rs`, returning a `ParkMode` of `Held` / `Engaged` /
`HeldByChildren` / `UntilCancelled` / `Quiesce`, `bus/inbox.rs`) is the `should_park`
verdict: a conversing node with a human attached parks `Held`, immune to
cancellation — unless its own token has been terminated (the one self-check
not answered by a `Weak::upgrade`), which a shutdown signal stamps, since a
conversing root has no parent to prune it away and parking `Held` past that
stamp would be a zombie; an unattended conversing root falls through to the
fleet arms, so it holds for live children and quiesces once they settle — the
exchange synod drives ends at fleet quiescence with no policy hook; a
returning, parented agent that is *messageable* — someone has messaged it, or
it holds a deposited reply — parks `Engaged`, the same wait except a
terminate-class cause still ends it, since an exchange is not a conversation;
busy children hold it until they reply or die; a live self-schedule holds it
until cancelled; otherwise it terminates at quiescence — the one-shot
contract a headless root satisfies. `--chat` builds the trunk with no system
prompt and no tool at all (an empty `Toolset`), so its `Nudges` steer
nothing — a bare conversation, the same attend loop.

## The attend loop

Three nested loops, the same for root and child alike:

- `attend` — the per-agent lifetime. The process hears the OS signals
  (`signals::face(&fleet)`, held at each site that launches one —
  `headless::run`, `headless::converse_settled`, and `tui::tui_loop::run` — for
  the whole run), which forwards each to every live root's `Agent::interrupt`
  or `Agent::cancel`; a sub-agent hears none, being reached through the tree
  cascade (`Agent::cancel_tree`). `attend` and its bounded twin
  `attend_backlog` (converse's per-exchange drain) are one loop,
  `attend_until`, differing in how an empty inbox is treated (`Idle::Park` /
  `Idle::Return`). Each pass draws the next item from this agent's
  [[map/exarch/frontend|inbox]] — parked, via
  `next_or_idle(|engaged| self.park_mode(engaged), …)`, which **re-evaluates
  the park verdict on every `Condvar` wake**, so the idle lease's
  terminate-cause cancel, or the last live child settling, is seen on the very
  next wake — and hands it to `take_up`, which answers a `Step { settled:
  Option<AgentOutcome>, flow: ControlFlow }` or the engine's `Severed`: every
  item reaching here already survived the inbox's own pop-time fence, so what
  is left is the exchange-boundary latch reset, a session command's run
  (`Avatar::read`, `Avatar::rewrite`), the item's `arrive` (booked against
  the park verdict and announced, `agent/arrival.rs`, the same door a
  mid-exchange arrival takes), its `attempt` (`deliberate` under
  `catch_unwind`, the log quiesced however it ended), `record_failure` (a
  provider error recorded, a refusal's resume armed), and the nudge reaction,
  `follow_up`. A genuine exchange boundary resets the nudge budget and
  clears the sticky cancel token (`cancel::Token::reset`), so a prior
  exchange's Esc cannot bleed into the next; a self-nudge is the same exchange
  continuing and resets neither. The budget counts *consecutive* repairs, so
  `deliberate` also clears it after every completed tool round: faults
  scattered through a long headless run, which is one exchange, never add up to
  one that ends it. Every `reply` deposits its value on the
  agent itself (`Agent::deposit_reply`, which also posts the one-line notice
  to a parent); a child then parks `Engaged`, and only a parentless returning
  agent — the headless root — ends its loop on it, its driver reading the
  deposit off `Agent::reply`. The loop's outcome is its last settlement; one
  that settled nothing ends on why it stopped drawing — `Cancelled` under a
  terminate cause (a lease reap, a `cancel`), a finish without `reply`
  otherwise. At the single exit the loop winds a stranded
  prompt back through `quiesce` so the next `append_user` is always admissible
  ([[invariants/turn-ends-ready|exchange-ends-ready]]); a root's `Avatar`
  then drops (a child's drops when its detached thread returns), pruned from
  its parent's `children` and the fleet's indices at their next walk — the
  whole of deregistration — so the fleet empties as its
  last agent's avatar goes. A panic `attempt` catches around its
  `deliberate` call, `Fault::Panicked`, is a
  *host-side* fault (provider transport, surface decode, render, digest),
  recorded as `AgentOutcome::Failed`; an eval-side panic never unwinds this far —
  the engine's own run door (`Shell::run`) checkpoints `env` / `context`
  at entry, rolls them back, and reports the failed run,
  durability being engine-owned ([[decisions/260612_exarch-panic-recovery|panic-recovery]]).
  The per-call host — `RunHost`, wrapping the desk and applier
  ([[map/exarch/shell-eval|shell-eval]]) — is never installed as shared
  state: it is a plain `Arc` `Avatar::ral` builds and holds on its own
  stack, so a panic unwinding through `deliberate` drops it with everything
  else there, with nothing separate to retire.
- `deliberate` — one prompt driven to quiescence over the agent's *own* provider
  (`self.provider.current()`, read once by `attempt` so a `/model` swap
  lands next item, never mid-deliberation), answering `Result<Outcome, Fault>`
  where `Fault` is the provider's own error, the engine's `Severed`, a log
  append refused, or a host-side panic. Both types live in the leaf
  `agent/outcome.rs`, which `attend` and `nudge` read without depending on
  the loop. `deliberate` itself is a short driver: weigh the context for an
  eviction (`must_evict`), take one `round_trip`, and act on the `Turn` it
  answers — `Ends(Outcome)`, `Calls`, or `Retry` after an overflow that
  `relieve` answered with an eviction, never twice running. `round_trip`
  records the turn's start, renders the context and streams a reply through
  `provider.complete`; `settle_turn` records its usage, **admits** then
  appends the assistant message, and reads the turn's verdict off its cut and
  its calls; `answer` runs the batch (`run_batch`), appends the results, takes
  in the mid-exchange arrivals and the newly climbed warnings as steering, and
  ends the deliberation on a cancel or a staged `reply`. The admission step (`admit_assistant`,
  at the commit boundary) enforces the [[invariants/transcript-admission|transcript-admission invariant]]:
  it repairs a non-object tool-call `fn_arguments` to `{}` (X2) and substitutes a
  stub for an otherwise-empty assistant message (X7), so every committed message
  serialises to a request a strict backend accepts. `StopReason::MaxTokens`
  *with no captured tool call* raises `ProviderError::Truncated` after appending
  the partial, so a `continue` nudge keeps the work as context (a stall's
  partial is prose, since reasoning alone is retried below the transcript, and
  its nudge says the connection failed rather than that the output was capped); *with* captured
  tool calls it dispatches them and continues instead, since returning `Truncated`
  there would strand the protocol in `AwaitingToolResults` and fail the nudge's
  next `append_user` (X6). Nothing bounds the number of round-trips: a
  deliberation ends when the model stops calling tools, or on a cancel or a
  provider error.
- **A severed seat runs nothing more.** `attend` breaks at the first
  severance it sees and every park quiesces behind one; `run_batch` answers
  each call left in the batch with the `EngineLost` sentence, and
  `deliberate` ends the exchange `Err(Fault::Severed)`, which `take_up`
  passes up as its own `Err` — one `?` where there were eight polls.
  `attend_until` records the loss once — the plain sentence as the failed
  outcome, the logged form (code and the engine's own words) as a durable
  note — and headless and [[map/synod|synod]] end the conversation on it.
- `run_batch` — runs one turn's tool-call batch in order, each call through
  `invoke`. Every call returns a
  `SessionToolResult` synchronously — dispatched through the launch's
  `Toolset` (`ral`, and `thinking` under `--thinking-tool`,
  [[map/exarch/tools|tools]]); a spawn verb inside `ral` hands the script a start
  receipt after launching the child. Once every requested tool id has
  a result,
  `run_batch` drains this agent's [[map/exarch/frontend|inbox]]'s tool-boundary
  steering. A non-slash steering prompt is appended after the complete
  tool-result batch, and the next turn asks the provider with the user's steering
  in context
  ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]]). A
  sub-agent has no human writer, so its inbox holds no steering — but the
  channel is never idle for it either: the context-pressure reminder joins that
  same one admitted message
  ([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]).

  A streamed turn closes through one door, `commit::Stream::close`: the stream
  keeps the first record that failed (a streaming callback has no error
  channel), seals the answer and reasoning choppers, then publishes
  `Transient::Boundary`, even when the provider or cancellation path already
  carries an error. On such an exit the close is best-effort (`abandoned`), so
  a live display edge cannot survive into the next turn
  ([[map/exarch/frontend|frontend]]).

Worker-reap and large-binding notices need no drain at `attend`'s top:
core's own engine pushes both as `` `notice `` surface classes at the ready
boundary of the run that produced them
([[map/core/engine-protocol|engine-protocol]]), decoded by
[[map/exarch/shell-eval|shell-eval]]'s `decode_surface` into `Surface::Notice`
and recorded from there. A reap notice names a worker removed by policy — the lease
chain's idle or backstop bound on a running worker, or the retention sweep
expiring a settled entry's unclaimed result — rather than one an eliminator
observed away. It is recorded as `Forensic::Reap` and drawn nowhere — a
worker nobody watched is not news — and never model-facing, since delivery of
a reap to the model itself is deferred.
`attend`'s top runs nothing else; the disk ceiling is weighed at the tool
boundary with the other gauges (below). (No pin is protected or reconciled —
[[design/pins|pins]].)

The retention clock itself is core's: the engine ticks the worker registry
once per source dispatch and sweeps it at each settled run's ready
boundary ([[map/core/shell-state|shell-state]]), armed with
[[map/exarch/shell-eval|shell-eval]]'s `SETTLED_WORKER_RETENTION`. The
agent keeps no mirror of that drum: the engine answers the question itself,
each `WorkerRow` the `` `workers `` probe returns carrying `retention_left` —
the calls until a settled entry expires, on the engine's own clock — which
`/resources` renders as the nearest expiry. Retention notices need no
plumbing of their own: they ride the same drain above.

**The binding-lease ledger** is armed by `shell_eval::arm_session_ledgers` —
the one policy site, run by the one root recipe (`boot::engine_root`) right after
the `Root`'s env is seeded as bindings (seeding then arming stay one visible
sequence); a fork keeps those terms through core's `Shell::bequeath` and
`Shell::child_of`, sealing everything it inherited ([[map/core/shell-state|shell-state]]) — with
[[map/exarch/shell-eval|shell-eval]]'s `BINDING_IDLE_CALLS` (256) and
`LARGE_BINDING_BYTES` (1 MiB), beside `arm_worker_retention`. The
large-binding residency nudge rides the pushed `` `notice `` channel above,
and the prune half is engine housekeeping too: idle top-level names fall at
the engine's own ready boundary, announced as a pushed
`` `notice `prune [...] `` class (core's `Notice`) the host records as `Forensic::Prune`
— kept in the log, drawn by no fold. The engine's
run-entry checkpoint orders after any prior boundary's prune, so a later
panic rollback can never resurrect a name a pass just pruned.

`/resources` is the probe fold over the same accumulators
([[invariants/probe-convention|probe-convention]]): a `Read` drained at the
next boundary, tool or exchange, and run by `Avatar::read` on the attend
thread that owns the agent —
`Avatar::resource_rows` surveys what this thread may legally read — the worker
registry's running/settled split with the nearest time-to-reap and the
binding-ledger figures read as *data* over the transport's probe rail
(`Frame::Probe`, through core's typed `Transport` probe doors and `Seat::read`,
which severs the seat on a refused answer,
[[map/core/engine-protocol|engine-protocol]]), plus inbox depth per
source, the event ledger's logical length and history bytes, the log dir
walked host-side and the scratch sized engine-side (`Transport::path_bytes`) at
invocation, and the sub-agent idle lease as two rows (nearest
time-to-reap) — and `emit_resources` posts one `Transient::Resources`
carrying the rendered card — chrome only, so unlike a recorded observation
there is no later fold to re-render one from.  A row's `policy` is the
closed `resources::Policy` enum, rendered as it reads.
A probe fold is an interactive diagnostic, read when it is run: no session
keeps a pressure history, so the figures live only in this emission, never
in `record.jsonl`. The frontend appends the rows for the accumulators *it*
owns (scrollbacks, views, the bus, the demote threshold — `tui/resources.rs`)
at render time; neither half reaches across a thread, and the agent module
never names the TUI. Probing never mutates and never renews a lease —
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
queue, uncapped — the one exception to the long-session-budgets principle
that every accumulator needs a bound: none of them is machine-floodable — a
child or worker posts its result once, a slash command arrives at a human's
typing rate, and a `message` costs its sender a model turn that fuel already
bounds — so a cap could fire only wrongly, and when it did the child would
park with its reply staged but its parent never notified, the very silent
loss a cap is meant to rule out.

There is no headless-completion gate and no role flag beside `parent`.
Nudging splits into two disciplines, named and owned by `agent/nudge.rs`'s
`Nudges` (`Avatar::readings.nudges`, which `steers` nothing for a toolless
`--chat` trunk): a
**repair** — `Empty`, `Stopped`, `Truncated`, or a returning agent's
`Complete` without `reply` — is event-shaped, exceptional, and spends the
consecutive-repair budget (`BUDGET`, 3); a **standing condition** — the pin
register here, and the ladder conditions of `agent/gauge.rs` below — is a fact
about the agent's own live state, **edge-triggered**: told once when it
changes, silent while it holds, budget-free.

**The two kinds ride two channels, because they are owed at different
boundaries.** `react`, the post-deliberation entry point, composes the repairs
and the pin reminder into at most one `EXARCH_REMINDER` message per
completion, self-posted as `Post::Nudge` and committed by `append_user` inside
the same exchange, gated on `quiet` — no standing reply, no running
worker, no busy children, the one condition those kinds share.
`Nudges::remind` is the other entry point, reached from `deliberate`'s `tell` at
a *tool* boundary, where a gauge's `Reminder` — figures, not words; every
model-facing sentence is `nudge.rs`'s to word — lands as steering trailing
the batch's arrivals: an agentic run takes one prompt and then two
hundred tool turns, so a warning that waited for the next completion would
arrive after the cut ([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]).
Its breadcrumb records no budget (`Forensic::Nudge { cause, spent: None }`);
a repair's records what it spent (`spent: Some(Spent { used, max })`).

| kind | channel | trigger | budget |
|---|---|---|---|
| empty-turn repair | `react` | quiet ∧ `Ok(Empty)` | spends |
| early-stop repair | `react` | quiet ∧ `Ok(Stopped)` | spends |
| truncation repair | `react` | quiet ∧ `Err(Truncated)` | spends |
| reply repair | `react` | quiet ∧ `Ok(Complete)` ∧ `must_reply` | spends |
| pin reminder | `react` | quiet ∧ `Ok(Complete)` ∧ register non-empty ∧ digest changed | free |
| pressure reminder | steering | gauge `Over` ∧ excursion untold | free |
| usage reminder | steering | an allowance window climbs past 90 or 95% | free |

The pressure reminder names the set it is warning about: the pressure reading
calls `Avatar::planned_eviction` — a walk back over the turn table's own
summed weights, no rendering — and carries the answer as
`Pressure::Over { detail, planned }`, so the message tells the model which
*turns* the next boundary would take, rendered as runs, that the material
stays readable with `exarch-transcript`, and how to make the same cut with a line of
its own — `` exarch-context `evict [turns: !{range a b}, note: `some '…'] `` — before they
go. With nothing old enough to shed, `planned` is `None` and the
reading alone is the whole message. Durability is the log's job, so the nudge
never asks the model to write state to files.

Everything else is accepted as-is, unspent, and deliberately so: `Replied` (a
reply is final), `Cancelled` (the human asked), and every unclassified
provider error (the transport's own).

`Nudges` owns the repair budget `used` and the pin digest last told
(`pinned_told`); the ladder latches are `gauge::Gauges`' (below), and the
two sit together with the token `Measure` in `Avatar::readings` — what the
avatar has measured and told about one context.
The pressure reading (`gauge.rs`'s `Avatar::pressure`) is a pure `&self` reading with no latch of its own, and
`deliberate` neither latches nor unlatches around it. An edge is **consumed
at decide time, in the same act as the part's emission**, so "told but not
sent" and "sent but not told" have no spelling. `reset()`, called on every
exchange-opening item, clears only the budget — a new exchange is not a new
condition, and clearing the edges there would re-tell an unchanged pin once
per delivered item. Every path that discards a decided-but-uncommitted nudge
instead rebuilds `Nudges` whole: `/clear` (`Inbox::clear` sweeps the queue,
`Avatar::clear` rebirths the whole `Readings`) and `/rewind` (`drop_nudges`
sheds the queued nudge, `rewind` rebirths `Nudges`) — the rebirth also covers a `/rewind` typed mid-
deliberation, whose `Barrier` queues ahead of the nudge `follow_up` posts
after it: the nudge is decided (its edges already consumed) and then shed,
so without the rebirth the telling would be recorded but never committed.

Edge-triggering the pin reminder closes a livelock: a stationary register can
produce at most one nudge, where a bare per-completion reminder would let a
`Complete`, its own reminder, and the next `Complete` cycle forever while the
register sat unchanged. A
standing condition staying *event-shaped* on the wire looks like it should
strain the fold law
([[decisions/260812_context-is-a-projection|context-is-a-projection]]'s "one
state, one fold") — a condition that *holds* seems like it should not be
re-stated per turn — but it does not: the transcript is already persistent,
so a committed turn stands in every later render until an edit sheds it, and
re-stating the register on every turn was never persistence, only
redundancy. What must be recorded is the level's *transitions*, and an edge —
"the register changed", "pressure crossed the line" — is genuinely
event-shaped, so recording it as a committed user turn is honest, not a
workaround. The alternatives are strictly worse against the law: a
render-time seam re-injecting live state would break `fold(log) == memo`,
since the model's view would stop being reproducible from the record; a
dedicated "standing condition" record class still has to enter the fold to
reach the model, so it is a user turn with a fancier name plus a new record
variant, fold arm, and admission rule — machinery for no semantic gain. An
eviction may carry a telling out of the context without harm either way: both
conditions self-heal regardless — pressure re-fires per excursion by
construction, the pin reminder re-fires on the next register change, and the
model can always `` exarch-pins `list ``.

A `--chat` trunk's `Nudges` **steers nothing** (`Nudges::new(false)`, the
launch's `Toolset` being empty): every nudge steers the model toward a
tool it does not have, so no rule runs, no reminder fires, and nothing
synthetic ever joins the conversation. Its provider errors still reach the
human, since that report is the attend loop's own step — `record_failure` emits
`Forensic::ProviderError` for whatever error the attempt carries before it
ever asks for a nudge, so `react` decides and nothing more.

**Ladder conditions** live in `agent/gauge.rs`: a reading climbs a ladder of
levels, each rung told once per excursion, and a fall below a rung re-arms it.
One `Latch` (`latch.rs`) serves every ladder: it keeps the level last weighed,
and `climb` answers the highest rung between that and the new level, so a first
reading past two rungs tells the higher alone; `cross` is the one-rung ladder
of a yes-or-no condition. A ladder is bare numbers, and who hears a rung is written
once, where its `Warning` is built. Every measurement the avatar takes of
itself lives in `gauge.rs` too: the token `Measure`, the eviction trigger and
plan, the scratch probe. Three ladders ride it; their latches are gathered in
`gauge::Gauges`, except the third's user latches, which live in `Rations`:

| condition | reading | rungs | audience |
|---|---|---|---|
| context pressure | `Avatar::pressure` (`Unknown` leaves the latch) | over the soft line | model |
| disk ceiling | `disk_warning`, walked at most once per `DISK_WALK_INTERVAL` (a minute) | over `disk_warn_bytes` | user |
| usage allowance | `Allowance::percent` of `Provider::allowances`, per account and window | user 50, 75, 90, 95% (`USER_HEARS`); model 90, 95% (`MODEL_HEARS`) | user once per account; model once per agent |

`Avatar::climbed` weighs all three after every tool batch, and `tell` delivers
each climbed rung: a user line as a `Forensic::SystemNote`, a model
`Reminder` worded by `Nudges::remind` into the steering message — nothing for
a `--chat` trunk, which reaches no tool boundary anyway. The ration gauge reads nothing
itself: the read is the provider's, spawned on the road every request takes
and shared by every agent on the account
([[map/exarch/provider|provider]]'s `Rations`). The user's latches are
`Rations`' too (`Provider::climb_allowances`, on `USER_HEARS`), so a rung is
told once per account by whichever agent climbs it first; the model's are each
agent's own `gauge::ration::Ration`, since each model needs its own reminder,
and its outcome sentence reads `Avatar::resumes_at_reset` — the trunk, under
a launch that resumes: paused until the reset and resumed, or refused until
it. The network is read once per account; the user hears once per account,
each conversation's model once.

A refusal whose reset lies past the in-place wait ends as a provider error:
on the terminal trunk alone (`resumes_at_reset`), `record_failure` answers
`ProviderError::Refused` with `resume_at`, which replaces any `provider-reset`
schedule with a one-shot wakeup at the reset and notes the time to the user.
Headless, synod and every fork fail instead. Once one agent's request is
refused, `Provider::complete` refuses every later request the hold covers
(the account for an allowance, one model for a rate) before sending
([[internals/provider-fault-recovery|provider-fault-recovery]]).

## The Fleet

`agent/fleet.rs`'s `Fleet` is `{ names: Mutex<HashMap<String, Slot>>, roots:
Mutex<Vec<Weak<Agent>>>, exits, departures, lease: Duration, launch: Launch }`
— two indices (`Slot` is `Claimed` or `Live(Weak<Agent>)`), a mutex and
condvar that wake `wait_empty`, the idle-lease bound, and the **`Launch`**: what every node of one
run shares, fixed once — `trunk` (how every root is driven; `trunk.attended()`
is whether a human types into it), `allow_schedule`, `resume_on_reset`,
the `Toolset`, `disk_warn_bytes`, the `factory`, the `Bureau`, the prompt
template with the `BuiltinIndex` each node resolves it against, and the
recipe for a root: the invocation `caps`, `fuel`, `search`, `sessions_root`
and `genesis`. Held here
once rather than copied into every `Agent`, so a fork's `Build` names only
what is its own. It is not the tree: the tree is
`Role::Child`/`Agent::children`, and every walk — the roster, the cancel
cascade, the scope check — runs there. `names` is the by-name door a spawn
claims identity at (`Fleet::claim`) and `` exarch-agents `message ``/`` `cancel ``/`` `read ``
resolve through (`Fleet::resolve`); `roots` holds every root session —
a root reports to nobody, so a walk from `roots`
(`nearest_reap`) is how the idle-lease scan reaches every live agent in the
run.  The frontend reaches an agent through the fleet: a tab resolves its
name with `Fleet::resolve` and checks the id, never holding the `Arc<Agent>`
past one handler, and `/quit` and the signal forwarder walk `Fleet::roots`
([[map/exarch/frontend|frontend]]). Both indices are `Weak`, and a lookup prunes what has
settled as it walks. `FleetBus` and the `Arc<provider::Engine>` are not
fields of `Fleet` — the frontend builds its own (`FleetBus::session`, one
emitter per root stamping that root's own mailbox) and holds the engine handle
separately; what every node
shares through `Fleet` is only identity resolution and the lease.

- **A root is born from `Genesis`.** `Launch::genesis` — installers, cwd,
  terminal capabilities, the run's `Scratch` — is `Some` on an identity host
  and `None` on a wire host, which boots no further engine. `Genesis::seat`
  is `Seat::root`, so the first root, a `/new` or `/branch` root and a
  resumed root each get a fresh engine the same way; the scratch is the
  run's and outlives every root and every `/clear`. `Launch::seat()` is the
  door that refuses, in a sentence, a host with no genesis.
- **Two steps: `Fleet::claim`, then `Fleet::enrol`.** A name is claimed
  before any log or engine seat exists (by each root's caller ahead of
  `Avatar::hatch`, by the desk's spawn path for a child), under the one lock a racing spawn's own claim takes:
  `check_name` (`NameMalformed`, so a wire peer cannot go around it) and
  the taken test (`NameTaken`) are decided there, and the name is held as
  `Slot::Claimed`. The `Claim` rides the `Build` and `enrol` consumes it,
  turning the slot `Live`, so no agent is enrolled under an unclaimed name;
  one dropped unconsumed gives its name back. The one remaining refusal,
  `Unborn::SessionDead` (the parent raced a cancel), is decided in `enrol`.
  A refused agent never appears in the fleet, however briefly: only after
  the name is live is the agent adopted into its parent's `children` or
  pushed onto `roots`. Birth arms no reply lease.
- **The fleet is alive while `roots` prunes to empty** — the literal "dies
  when no active agents remain" (`Fleet::is_empty`; `Fleet::wait_empty`
  blocks on it, woken by `Fleet::departed`, which `spawn_async`'s worker calls
  once its avatar has fully dropped). An agent leaves by its
  avatar dropping (quiescence, cancel, a non-reply finish); a conversing
  node stays until `/quit` or `/close` because it parks, and the process ends
  with its last root; a headless root leaves at quiescence; a replied sub-agent parks under its idle lease and
  leaves when it is reaped. There is no human-less daemon: nothing lingers
  without a present human, running work, a bounded lease, or a bounded
  self-schedule.
- **The follow-up lease starts at reply.** `Agent::deposit_reply` records
  `Status::replied_at` and calls `Fleet::lease_reply` for a child only. After
  `AGENT_LEASE_IDLE` (one hour), `lease_fire` cancels the subtree with
  `CancelCause::TimedOut` only if that reply's timestamp still matches.
  `deliberate` clears it through `disarm_reply_lease` when model work resumes;
  read-only commands leave it alone. `reply_since` also checks
  the inbox exchange clock so a queued human or agent message protects the
  child before the loop takes it up. A later reply arms a fresh deadline;
  an old callback cannot reap that new waiting period. Working children and
  roots have no reply lease. `nearest_reap` reports only active reply leases.
  Reading a reply, focusing a tab, or probing resources renews nothing.

## Cancellation cascades the subtree, across both layers

The single cascade serves the deliberate teardowns — `` exarch-agents `cancel ``, a
returning agent's `reply`, and the `/clear` / `/close` subtree reaps — and it
runs over the agent tree itself, not a registry. `Agent::name` is its
identity — well-formed and unique among live agents, both enforced at
`Fleet::claim` (`Unborn::NameMalformed`/`NameTaken`; the trunk holds
`TRUNK_NAME`, `agent/build.rs`), which is where a name *becomes* identity and
so the one place a wire peer cannot go around. Both doors claim before they
fork a log or dial anything, and `enrol` takes the claim, so the check is
race-free and there is no second, advisory one. A name is also the handle `` exarch-agents `message ``/`` `cancel `` resolve by,
through `Fleet::resolve` — and then, for `` `cancel `` and `` `read `` alone,
the scope climb (`Agent::descendant`); a message needs no climb. The roster
(`agent/roster.rs`'s `listing`) walks the same tree instead — `Agent::walk` from
the reader's own root — never the name map, yet lands on the same set: every
enrolled agent is at once named and placed, so what a model can see through
`` `list `` is exactly what it can reach through `` `message ``/`` `cancel ``.
Every `Agent` carries a strong `parent`, so the tree
is the spawn tree directly: `Agent::cancel_tree` cancels an agent and its
whole subtree, `Agent::cancel_descendants` abandons a returning agent's
children, and `Agent::forget` reaps a subtree and forgets what it
was `awaiting`. The fence a late result or deferred surface batch must
survive is not here: `Avatar::clear` drains this agent's own inbox,
and that drain is what bumps the clear-epoch a `bus::Stamp`-borne message
is judged against at its own pop. The epoch is per inbox, not per fleet,
and the envelope a worker carries is its *reader's* by construction — a
`/clear` in one tab must not throw away work another tab is still waiting
on. Each cancelled node is
stopped **across both layers**:
its cooperative `Token` (read by `deliberate` between turns and raced by
the provider's mid-stream cancel) *and* the eval layer through its own
`reach: InterruptTarget` — the cell holding its seat's current `ControlSender`,
republished by `Seat::clear`, so it never goes stale — whose `interrupt` strikes
the dispatch in flight (even one still waiting on the engine lock,
[[internals/cancellation|cancellation]]) and whose `terminate` cancels the
engine's durable root, under either carrier alike — and a `ral` eval already
in flight unwinds at the evaluator's poll points instead of grinding to its
`timeout_secs` wall. No root carries a lease, so a `terminate` reaches a root only when the
process is ending or on its own `/close`. No engine faces the process's signals itself: `signals::face`
forwards each to every live root
([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]],
[[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]],
[[internals/cancellation|cancellation]]). `Esc` / Ctrl-C, by contrast, are a
**per-tab exchange interrupt**, not a cascade: they stop only the *focused* agent's
current exchange (`Agent::interrupt`, reached through the focused tab's own
`Weak`), leaving its descendants running
([[decisions/260705_cancel-per-tab|cancel-per-tab]]); the focused agent's
sticky token is cleared at each exchange boundary (`Token::reset`).

Cancelling the durable root already reaches a cancelled node's own `ral`
workers with no edge of its own: a worker's cancel scope is a child of its
shell's durable root, and every `Scope::is_cancelled` walks its
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
parameters. `attempt` reads `self.provider.current()` once per item. `/model` swaps
the **focused** agent's handle directly on the UI thread (resolved through
the fleet by id), so a
swap on one agent never disturbs another. `fork` seeds the child's own handle
from the parent's current provider (`ProviderHandle::new(self.provider.current())`),
so the child inherits the model in force at spawn and may diverge afterward.

A builtin spawn may say otherwise. `` exarch-agents `start ``'s `model` field
names `` `inherit `` or `` `role <Str> ``, and `ExarchDesk::child_provider`
reads it **before the `SeatKind` split**, so both arms share one resolution and
a refusal unwinds nothing — no adopted fork, no forked log, no engine made.
`` `inherit `` shares the parent's `Arc<Provider>`, allocating nothing; a role
is the `Arc<Provider>` the human bound to that name at launch
(`Launch::roles`, [[map/exarch/provider|provider]]), minted once with the
launch's tuning and output cap. With no roles configured, or an unknown name,
the spawn is refused saying so. The agent names neither a provider nor a model,
and a `Roles` prompt section lists the roles to an agent that spawns, only when
any exist.

The one `AgentLog::fork` call site — `Avatar::child_of`, which the desk's
`start` arm calls — hands the child log the *live* model and account rather than copying the parent log's,
which were fixed at its session start: a child forked after a `/model` on the
parent's tab records what it actually runs, not the pre-switch model. A root
hatched by `/new` or `/branch` likewise starts on the focused agent's current
provider.

`AgentLog` authors `Protocol` and its own bookends and nothing else: its
protocol mutations go through one funnel that emits and then advances the model
fold, and every other fact (usage, nudges, errors, `TurnStarted`, session
end) is emitted by its worker through the agent's `Emitter`, with
`emit_or_report` where a failure is a fault rather than an error. `fork`
(`pub(crate)`) takes the child's `system` prompt and a `Memory`, and a `mnemon`
child's log opens with `SessionStarted`, `Inherited`, `System` in that order;
the `/new` and `/branch` roots (`spawn::hatch_root`) pass the prompt they built
and, for `/branch`, the `Inherited` they computed.

## Lifecycle: clear, close, evict, resume, hatch, fork

`clear` rebuilds the focused agent without carrying cancellation residue forward:
it drops the waiting inbox before the reboot, so a prompt typed during that
reboot belongs to the new context, then reboots the seat (`Seat::clear`) — an
identity root boots a fresh engine through `IdentityTransport::boot` from the
same `Attach`, so the scratch it names and the prompt that names it stay true,
onto the *same* interrupt target, and resets the escalation ladder; a wire
seat or an adopted fork has no recipe to reboot in place and answers `/clear`
with a sentence. It then has the agent `forget` — cancel
its subtree, drop what it was awaiting, clear its wakeups and its pins — and
records `Protocol::Cleared` through `AgentLog::clear`, in the same
`record.jsonl` and through the same seam: every turn is *withdrawn* from the
context, the system prompt stays, and the log keeps every record, so the cleared
turns stay readable through `exarch-transcript`. `clear` refuses while tool
results are outstanding, and a cleared child keeps its parentage. The frontend
learns of the clear from the record itself, its mirror stepping `Delta::Cleared`.
Clear then rebirths the avatar's `Readings` whole, a rebuilt context having been
measured and told nothing. It touches no process-wide state: a `/clear` on
one tab is that tab's. Replacing the
transport drops the outgoing shell, whose `LocalState` teardown cancels
every worker still registered on it — explicit destruction outranks every
lease, the durable class included, with no host call site to forget
([[map/core/shell-state|shell-state]]). A worker settling after the cancel still tries to flush
its deferred `done` batch through the boundary it captured before the clear;
the same pop-time epoch fence ([[map/exarch/shell-eval|shell-eval]]) that
already drops a stale agent result drops that flush too, so no pre-clear
worker output survives into the rebuilt context. It is the focused agent's,
not a fleet-wide reset.

`--resume` is a root-only lifecycle, `Avatar::resume`: it reopens every live
root of the run, each from its own `record.jsonl` (`AgentLog::resume`, which
accepts any root head, not session 0 alone), validating and replaying it,
quarantining only an unterminated crash fragment, then reopening the file for
append. The live model and provider are selected again, the freshly built
prompt is recorded as `Protocol::System` iff it differs from the one in force
(`AgentLog::record_resumed`), while each shell is
fresh and receives a note, imported as a `Protocol::Import` turn, describing the
bindings, workers, cwd, scratch, pins, and schedules that were not durable.
A `mnemon` root's record, headed by `Protocol::Inherited`, folds back like any
other by reading its resident rows through the pointers the link carries, so a
resumed `/branch` root needs its source session's `record.jsonl`; an unreadable
source refuses the resume, naming the file. Wire seats, chat runs and child
logs are refused rather than half-resumed; a headless `--resume` takes exactly
one live root and otherwise refuses, naming the count — the only place roots
are counted.

A session ends by one of two rewrites, each run at the exchange boundary.
`Rewrite::Quit` breaks the attend loop and nothing more, so a resume reopens
the root; the TUI posts it to every live root. `Rewrite::Close` cancels the
descendants, records `Forensic::SessionClosed` and then breaks, so the
session is gone for good; the TUI posts it to the focused agent alone.
`SessionEnded` is written on every drop and is not a close. The TUI
interrupts the agent before either, and on a returning child `Close` simply
ends it, the parent receiving the usual non-reply notice and a resume
ignoring the record.

`evict` sheds the older half of the context when pressure crosses the context
window's reserve (`gauge.rs`'s `eviction_due` — used tokens into the top 15%
of a known window; `EVICT_THRESHOLD`, 500 KiB of serialised history, is the
fallback when the window is unknown) and `Context::can_evict` holds (no
pending tool results). It is called **at the top of every `deliberate` loop
iteration**, before the `TurnStarted` record: every turn boundary is a boundary the
protocol is well-formed at, and the turn a cut would take is never the one in
hand, so there is no iteration where weighing is unsafe
([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]). Long
autonomous and headless sessions therefore stay bounded *within* one
deliberation, without an interactive `/evict`.
`suffix_keep_budget` (half the history bytes) sets the cut and
`plan_eviction(keep)` walks back over the turn table's summed weights until
that budget is spent, answering the **set** the cut would take —
`Option<Vec<u64>>`, `None` when nothing is old enough. The walk's candidates
are every resident turn but the last, so the work in hand is never in a plan's
set — the budget alone would cut everything when the newest turn outweighs the
whole of it. A prompt whose answers still hold a resident turn outside the set
stays with them, so a cut that reaches into a prompt's answers keeps the
prompt. **No provider call is
involved**, so a boundary Esc has nothing to interrupt and simply leaves the
context as it lies
([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).

A successful eviction is `evict(turns, None, EditAuthority::Harness)`, which
records `Protocol::Evicted { cut: Cut { turns, note: None }, by }` — the
*resolved* set, the ids that actually left, so replay departs exactly those and
re-derives nothing
([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]])
— wrapped in `Transient::State(AgentState::Evicting)`
for the state row — momentary, since the edit is the whole of it. The same fold
step marks the departing turns `Held::Evicted` and frees their resident
records, retaining only their byte ranges in `record.jsonl`; this is residency
following the context, not a second drain rule. `render_marker` draws one
marker at each hole the cut opens, from those very turns and the `Cut` beside
them, so what left is still named
and still readable through `exarch-transcript`. Nothing old enough to shed is a no-op,
not an event. The durable log is appended to, never rewritten.

Harness eviction is one authority over the one context edit
([[decisions/260812_context-is-a-projection|context-is-a-projection]]). The
model reaches the other two: `` exarch-context `survey ``/``
`evict [turns, note] `` survey and edit the context and `exarch-transcript`'s ``
`index ``/`` `read ``/`` `grep `` read the record, an edit recording
`Protocol::Evicted` with `EditAuthority::Model` rather than `Harness`; only the
model's own `` `evict `` carries a `note`. The user's own hand is
`/rewind`, a picker over the prompts on screen ([[map/exarch/frontend|frontend]]),
which is **not an eviction**: `AgentLog::rewind(anchor)`
records `Protocol::Rewound { anchor }` alone, and the fold step *withdraws* the
anchor and every later turn — departed rows included — from the context, with no
hole and no marker. The turns stay readable through `exarch-transcript`, the next
prompt is minted `reach + 1` so ids stay monotone, the automaton rests where the
kept prefix leaves it (`state::resting`), and an anchor already withdrawn is
refused. `Avatar::rewind` sheds
queued self-nudges and rebuilds the nudge state; the screen learns of the rewind
from the record. The shell is not rewound,
and the picker says so. `AgentLog::append_user(text, continues, voice)` decides
once whether a prompt opens a turn (`UserPrompt { turn, voice }`) or extends the
live one (`Steering`), and the view fold draws the prompt off that very record
iff its `Voice` is not `Harness`, so the screen knows exactly which turn each
prompt opened (`Item::voice`: `Human` and `Wakeup` as themselves, a peer
`Message` as `Peer`, and a child result, nudge or surface notice as `Harness`);
`/context` surveys the context without editing it — `emit_context_survey`
posts the survey's turns as a `Display::Context` fact, and `context_rows_card`
groups them off their roles at draw time, one field per prompt with the
resident turn range answering it and its weight — a `Read` beside
`/resources` and the session-opening `/new` and `/branch`
(`agent/command.rs`).

`Avatar::disk_warning` is the disk half of the same ADR ("Disk: report
and warn only") — report-and-warn only, never rotation or deletion.
Unconfigured (`config::disk_warn_bytes` absent, the default) it is a no-op
by construction: no walk, no cost, ever. Configured, it walks at most once
per `DISK_WALK_INTERVAL` (a minute, stamped on the `Gauges`, so a `/clear`
walks afresh),
weighed at the tool boundary with the other gauges.
Crossing the ceiling (the session log dir, sized host-side by
`path::tree::bytes`, plus `EXARCH_SCRATCH`, read and sized in the engine by
`Transport::env_var` and `Transport::path_bytes`) is told to the user once, as a
`Forensic::SystemNote`, on the disk ladder's `Latch` — one warning per
excursion, not one per boundary.

A fork builds the child `Avatar` (and, inside it, the child `Agent`) for
[[design/agents|sub-agent spawning]] out of a fork the *engine* makes of itself
(`Shell::bequeath`, [[map/core/shell-state|the flow matrix]]), never one
the host holds. `Avatar::child_of`, which the desk's `start` arm calls, builds
the child; no `Shell` crosses into exarch. The child runs under its parent's
`GrantStack`, and the one layer the spawn's `grant` names
([[map/exarch/policy|`policy::base_layer`]] for a base tag, `decode_capability_map`
for a `` `restrict `` record, nothing for `` `inherit `` —
[[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]) rides the
`Bequest` and is pushed engine-side by `Shell::child_of`, through
`SpawnGrant::narrow_onto`, whether the child is adopted by
`IdentityTransport::adopt_parked` or sealed and made by the host's `factory`.
A returning child sets `parent: Some(..)` — the strong tree edge that
routes its result and drives the subtree cascade — and enrols itself in the
fleet (`Fleet::enrol`), joining the shared `Arc<Fleet>` every node holds.
It snapshots the **serialisable fragment** of the parent's lexical scope —
prelude, agent library, every accumulated binding — with its dynamic context
(cwd, env, grants, handlers) and the installed builtin table, and starts fresh
in everything else: fresh control counters and a freshly-defaulted
`SessionState`, so it holds **no terminal authority** (`TerminalAccess::Denied`,
no lease — a sub-agent is not the foreground agent and can never seize the
controlling terminal the TUI owns).

- **The fork.** The spawn builtin (`fork_then_enquire`,
  `shell_eval/builtins/harness.rs`) takes `Shell::bequeath(grant)`, the
  one door both spawn arms take ([[design/agents|agents]]'s one snapshot law):
  the identity arm parks the `Bequest` and the handler redeems it with
  `adopt_parked(id)`, the wire arm seals it (`seed::seal`) and names the bytes in its enquiry. The grant
  rides the bequest; the desk does not read `spec.grant`. The scrub replaces
  each `Value::Handle` the bequest reaches with an
  `` `opaque `` placeholder — in a session binding, in any scope a closure
  captured, in a native's `applied` arguments, in a handler frame's arms — the
  name staying bound, and a bequest carries no hooks
  ([[map/core/shell-state|the flow matrix]]). Closures stay live, and a parent
  that reaches no handle bequeaths its scope shared, not copied.
- **The lease.** `Shell::construct` arms the child's ledger from the
  bequest's terms, keeping the parent's lease terms and sealing every
  inherited name as baseline on either seat: inherited scratch is never
  pruned in the child (`fork_child_inherited_scratch_is_baseline`).
There is no flow-back: the child's `cd`, env, and new bindings die with it. An
agent with fuel left may spawn, and each fork hands the child one less unit of
`fuel` than the parent holds (`SPAWN_FUEL = 3` at a root; the parent's own
fuel is never debited, so fuel bounds depth, not fan-out). At `fuel == 0` the
prompt drops the spawn family — `exarch-agents` — and the desk refuses
`` exarch-agents `start `` with the exhaustion text; the desk remains the runtime wall
([[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]]) — so a delegation
chain bottoms out by refusal a fixed number of generations down. The fork
mirrors on the bus as `Signal::Born` — sent by `spawn_async` minting the
child's emitter, before its worker thread exists, so it precedes everything the
child sends — and `Transient::Died`, regardless of remaining fuel; a worker
thread that cannot be spawned sends the `Died` itself.

`/new` and `/branch` take no fork at all: a root is not a child. `Read::Hatch
{ memory, name }`, run by `Avatar::read`, calls `spawn::spawn_root`, which mints
the name (the given one, else `root-{N}` off one process counter),
claims it, seats a new engine through `Launch::seat` and hatches the root on the focused
agent's current provider (`hatch_root`). The log opens as `AgentLog::root`, and
for `Mnemon` imports the focused agent's `inherited_context()`, the same import
a `mnemon` spawn makes, with a fresh shell. The root then goes to `spawn_async`
with no prompt, so it parks for the human; the focused tab's rail shows one
`/new` or `/branch` row. There is no engine-side branch door: no internal
builtin, no desk tag, so a model cannot open a session. A builtin spawn's
`` `start `` tag leaves its fork the other way, and the desk's
`` exarch-agents `start `` arm collects it and assembles it at one less unit
of fuel ([[map/exarch/builtins|builtins]]).

Prompt resolution is one function, `Launch::prompt_for`, shared by every
root (`Avatar::hatch`) and child (`Avatar::child_of`, either seat). The launch
keeps the unresolved base and the index; each node
applies its own `Reply` and child-fuel bits beside the launch's
`allow_schedule`; the resolver appends `Agents` iff fuel remains, `Spawned`
(`data/spawned.md`: the snapshot, the spawner binding the reply, follow-ups)
iff `ToSpawner`, and `Reply` (`data/reply.md`, true of any receiver) iff the
agent returns. The headless root thus never reads that it was spawned. The
child's log bookend records that fully resolved prompt length, including the
filtered index and late sections.

### Wire-seat spawn

**Both seats spawn in one exchange of the same `` exarch-agents `start ``
vocabulary; they differ only in where the fork waits.** The arm is chosen on a
stated fact — `HostServices::kind`, a `SeatKind::{Identity(parent transport),
Wire}` read off the seat when the desk's capture is built, once per `ral` call
(`agent/shell.rs`), matched against the receipt the enquiry carries
(`ExarchDesk::fork_seat`). What varies is the `fork` tag the
*engine* mints beside the model's own spec record, since the reentrancy law
bars a desk handler from holding the `&mut Shell` a fork needs:

- **Identity.** The run's `Fork` door is `Fork::Park(nursery)`, so the
  builtin body (`fork_then_enquire`) parks a
  scrubbed fork armed with the session ledgers and names the slot as
  `` `parked <id> ``. The desk adopts it by id through the parent's own
  transport (`IdentityTransport::adopt_parked`), in the same process.
- **Wire.** A guest engine's runs carry `Fork::Seal` (`core/src/engine.rs`):
  the builtin body does no I/O. It takes `Shell::bequeath`, `seal`s the
  bequest into bytes the host never reads, and the enquiry names them as
  `` `sealed <bytes> ``. No token, no ack, no seed fd and no table of children
  exist: no engine spawns an engine.

The desk's wire arm (`make_engine`, `desk/agents.rs`) asks the launch's
**`Factory`** (`exarch/src/agent/factory.rs` — `vm_manager`-free by
construction, a capability object `RootConfig` hands to the `Launch`, `None` on
every identity trunk) for one connection to a fresh engine, adopts it as a
`WireTransport`, and seats it with `Seat::wire(transport, Attach::child(engine, seal))`:
the parent's `EngineSpec` plus the seal, written together, then `await_attached`.
The seat stores only the `EngineSpec`, so no seat holds its parent's
bequest for its own life and a child attach has no cwd or env to get stale. The desk answers the parent's enquiry only after that
returns, so the child is attached before the roster names it, and hands the
seat to the same `spawn_async` an identity fork reaches, at `fuel = parent - 1`.
A refused attach reaches the parent as the desk's refusal, in the engine's
words. Synod's factory is the guest daemon on `FACTORY_PORT`, which admits only
`VMADDR_CID_HOST` and spawns one engine per connection ([[map/synod|synod]]);
the child unseals against its own `Runtime` in `Engine::boot`.

One exchange, one connection: the whole of failure is local. A factory that
cannot make an engine refuses in a sentence; past the attach a refusal simply
drops the stream, the child reads EOF on fd 3, exits, and the daemon reaps it.
A wire root with fuel > 0 and no factory is a construction error, refused at
`Fleet::found` with a sentence rather than discovered by a model calling
`agent`; on a backend that cannot dial inward (the HCS machine) the factory's
refusal says so in one sentence.
The enquiring builtin cannot tell which arm served it — both answer the same
roster `` exarch-agents `` gives every tag. See [[design/agents|agents]] for the
seed's isolation law and [[map/synod|synod]] for `MachineFactory`, the
`Factory` over `Machine::connect_guest`. The `` `mnemon `` memory mode
additionally forks
the parent's `AgentLog` and imports its model-visible context before assembly,
as `/branch` does for a root
([[decisions/260702_subagent-memory-modes|subagent-memory-modes]]); `AgentLog`
drops a pending unanswered assistant tool-call frame when the parent is
mid-dispatch, so the child inherits a request context rather than a dangling
provider protocol. Both memory modes seed the launch prompt through the
child's inbox, so the prompt enters through the same item path.

Routing the fork through core matters because the builtin table is the easiest
thing to drop. The exarch host builtins — `view-text`/`view-hash`, `grep-files`,
`edit-hash`, `edit-replace`, `explore-dir`, `find-files`, the skill loaders
([[map/exarch/builtins|builtins]]) — live in the agent's own dispatch table
rather than in the environment, and the
`view-text-around` helper in `agent.ral` calls `view-text`. A fork that copied
only the scope and the context would leave the child's `view-text-around`
resolving to nothing and falling through to a failed PATH lookup.
A session fork snapshots scope, context, *and* the builtin table as part of the
flow matrix, so the decision lives in one place and the table cannot be
silently severed at this call site.

`digest.rs` holds `clip` and the fixed per-section byte caps for what the
*model* sees in history: each tool-result section has its own cap
(`VALUE_CAP` 20 KiB, `STDOUT_CAP`/`STDERR_CAP` 10 KiB), alongside a separate cap
for opaque error blobs (`OPAQUE_CAP`); `clip` measures what a terminal would
show (`ral_core::ansi::visible`, replaying `\r` and backspace beside the
escape strip). The whole-history gauges — the `EVICT_THRESHOLD` byte
fallback, `eviction_due`/`eviction_trigger` (the reserve line and the token
count that names it, for `/resources`), `pressure_due` and its
`PRESSURE_THRESHOLD_FALLBACK` one reserve ahead of it, and
`suffix_keep_budget`, the half-the-bytes cut — are `gauge.rs`'s. These caps
bound a single result; eviction bounds the whole history. A child's reply is
not clipped, since it reaches the parent as a value through
`` exarch-agents `read `` rather than as text. An oversize section keeps a head+tail digest
and elides the middle, with a banner nudging the model to scope the query at
its source and re-read in slices — the same rendering the transcript records,
so the user never sees more of a result than the model does. A structured
`VALUE` arrives pre-budgeted — the printer spends its own byte budget inside
the value, cutting where a container can still name what it dropped — so `clip`
is a backstop there, while stdout, stderr, and raw payload strings still meet it
head-on.

Every `ral` result closes with a `TURN: <id>` line naming the turn it closes.
That is how the model learns its own turn number, and so the address it evicts
and reads by: the same id `` exarch-context `evict [turns: …] `` and
`` exarch-transcript `read [turns: …] `` take. It is appended in `agent/shell.rs`,
from `AgentLog::current_turn()`, not in `digest`'s rendering — the id is the
session's, known where the result is assembled and not inside a byte-capped
section, and `system.md` tells the model to expect it
([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]).

`Avatar::ral` here threads to [[map/exarch/shell-eval|shell-eval]]'s
`run_shell`, answering an `Evaluated { text, failed }`; its desk capture
(`HostServices`) is built from the avatar alone, with no probe per call.

## See also

[[design/agents|agents]] (the role model these nodes realise — one `parent`
predicate, the conversing trunk vs returning agents),
[[map/exarch/tools|tools]] (the `Toolset` a request advertises — `ral`, and
`thinking` under `--thinking-tool`) and [[map/exarch/builtins|builtins]] (the harness verbs the
desk answers), [[map/exarch/frontend|frontend]] (the bus, the inbox, the
fleet doors, and the two frontends), [[map/exarch/provider|provider]],
[[map/exarch/policy|policy]], [[map/exarch|exarch]],
[[design/residency|residency]] (the resident ledger this cascade and the
worker/schedule teardown edge are chapters of),
[[map/core/engine-protocol|engine-protocol]] (why a wire
seat is one engine process, one connection),
[[decisions/260827_agent-and-avatar|agent-and-avatar]] (why `Agent` and
`Avatar` split this way, what the three-copy shape it replaced deleted, and
the later amendment that moved the run's settings onto the fleet's `Launch`),
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] (the turn as
the unit of eviction, the loop-top weighing, and the pressure reminder's
channel),
[[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]
(the one edit, its three authorities, and the marker at each hole),
[[map/synod|synod]] (`MachineFactory`, and synod's own helper surface built over
the wire-seat spawn above).
