---
generated_at_commit: 2b930e85
generated_at_date: 2026-10-28
covers_paths: [exarch/src/agent.rs, exarch/src/latch.rs, exarch/src/agent/, exarch/src/app.rs, exarch/src/boot.rs, exarch/src/signals.rs, exarch/src/schedule.rs, exarch/src/enquiry.rs, exarch/src/prompt.rs, exarch/src/config.rs, exarch/src/net_policy.rs, exarch/src/net_policy/]
---

# Map: exarch / agent

**`Agent` is one attending thread's owner; `AgentHandle` is its shared
address.** One inbox holds the cross-thread state, and ral scopes determine
cancellation ([[decisions/261028_one-scope-tree-one-inbox|one-scope-tree-one-inbox]]).
The role and snapshot laws are [[design/agents|agents]]'s.

## Owner and address

`exarch/src/agent.rs` owns the node; `agent/handle.rs` owns its address.

- `Agent` owns `Inbox`, `LogCell`, `ReplyCell`, pins, schedules, `Readings`,
  the fleet and, last, its `Incarnation`: the ral `Engine` with its
  `Tenancy` and engine-incarnation scope. Runtime methods act on this owner,
  not a shared agent.
- `AgentHandle(Arc<Shared>)` holds identity, name, held log/run directories,
  start time, fixed `Role`, fuel, search, `ProviderHandle`, containing scope
  `under`, and a life-scoped `Mailbox`. It retains no owner or engine.
- `Role` is `Converses`, `Headless`, or `Child { parent: AgentHandle }`.
  The parent edge is strong and fixed. `root` and `descendant` climb it
  without locks; downward views filter the fleet's live handles by ancestry.
- `ProviderHandle::{current, swap}` is the shared model-selection register.
  `attempt` snapshots it once per item; a swap affects the next item, not a
  request already streaming. Child selection inherits that provider or
  resolves a launch-bound role before engine construction.
- `SharedCell`, `LogCell` and `ReplyCell` (`agent/shell.rs`) are
  single-thread cells: reentrant borrowing panics. Desks execute only on the
  owner thread; pipeline stages and workers have no desk.

## Inbox, work and scopes

**`bus/inbox.rs` owns one mutex and condvar, not the agent module.**

- `Inbox` is the non-cloneable consumer; `Mailbox` pairs destination with
  a vouching `Scope`. `WeakMailbox` lets timers retain no inbox owner.
- `State` holds `Posts`, current `Work` (scope and receipt pool), exchange
  and pending opening, open job and counter, debts taken up, the parent
  `Report`, the reply `Lease`, rest/reply clocks and waiting-for-input.
  `Status` is a snapshot, not a second state register.
- The scope tree is fleet root → life → work → exchange and adopted child
  lives. Scopes never reset. `AgentHandle::interrupt` cancels exchange and
  opening; `stop(JobId)` abandons matching job work; `end` cancels life.
- Private `Post` distinguishes human text, reads, rewrites and scoped
  deliveries. `Posts` is the only way to observe them, and every observation
  first drops a scoped delivery whose provenance is cancelled. `State::pop`
  then releases a result's receipts and takes up a message's debt atomically.
  [[map/exarch/frontend|Frontend]] maps ordering and coalescing.
- `Inbox::mailbox` captures current work. `Mailbox::adopt` accepts only
  that live work and returns the child's life node, `Report` and first
  `Owed`. A delayed construction cannot adopt into replacement work.
- An `Owed` is one answer promised to a parent, minted at adoption and at
  each follow-up (`Report::owe`, under the adopting parent's lock). Paying
  consumes it into a `Result` carrying its `Receipt`; dropping it pays `Cancelled`; a refused post
  withdraws it silently in `deliver`. Busy is exactly receipts outstanding
  for the current work, never a live-child intersection.
- `Agent::abandon` applies `Queue::{Keep, Recall}` under the inbox
  lock and clears schedules after unlocking. Reply keeps queued posts, Stop
  recalls human text. A committed `/clear` discards only its captured old
  queue boundary, retaining later human input. Stop itself runs
  through the handle and touches no owner register.
- Never hold two inboxes. The inbox may read the schedule registry's leaf
  lock; `fire_at` releases it before posting. Provider work, engine
  construction, probes, writes and destruction run outside inbox locks.

## The engine

**`agent/engine.rs` carries one ral engine under either transport.**

- `Engine::Identity` holds `Arc<IdentityTransport>` and optional `Rebirth`;
  a root's recipe retains installers, `Attach` and run scratch. An adopted
  fork has no reboot recipe.
- `Engine::Wire` holds `WireTransport` and `EngineSpec`, not a parent
  bequest. `Engine::wire` attaches and waits for `Attached` before returning.
- `transport`, `kind`, `read` and `severed` are the local facade;
  `Incarnation::install` alone installs the deferred sink. A rejected probe becomes terminal `Severed::Faulted`.
- `Engine::prepare_rebirth` admits only identity roots with `Rebirth` and
  boots from that same recipe without changing the live engine; wire and
  adopted engines refuse. `rebootable` is a test-only eligibility probe.
- `Incarnation` (same file) owns the `Tenancy` and the deferred-batch scope.
  `install_rebirth` fences that scope, then installs an already-booted
  engine through `Tenancy::install` and certifies the outgoing one.
  `Engine::end` is `Err` when teardown is uncertified.
- `RootEngine::{Identity, Wire}` (`agent/build.rs`) chooses construction;
  `RootEngine::guest` adopts the guest control stream with guest cwd/home
  and an unresolved ceiling. `app::SCRATCH_VAR` names `EXARCH_SCRATCH` in
  both namespaces. A fuelled wire root without `Factory` is refused.
- The provider's runtime and client cache are `provider::Pool`, not this
  ral engine ([[map/exarch/provider|provider]]).

`Agent::ral(exchange, …)` (`agent/shell.rs`) captures `HostServices` once,
including current work mailbox, stages the per-call `RunHost`, installs a
**engine-incarnation-vouched** deferred sink, then passes `Some(exchange)` to `run_shell`.
`dispatch_to_report` owns addressed cancellation; cancelling threads never
write to an engine. Loose ral workers belong to the engine and survive Stop
and reply; dropping the engine ends them
([[map/exarch/shell-eval|shell-eval]], [[internals/cancellation|cancellation]]).

## Construction and the fleet

**Names, liveness and teardown have separate witnesses.**
`agent/fleet.rs` holds the `Census` (names, tenants, teardown failures), the
fleet root, lease policy and `Tenancy`.

- `Launch` holds the fixed trunk policy, schedules/reset-resume admission,
  tools, disk ceiling, factory, bureau, roles, unresolved prompt/index,
  root fuel/search, held run/session directories and optional `Genesis`.
- `Genesis::engine` is the identity-root recipe: installers, cwd, grants,
  terminal state and run scratch. `Launch::engine` refuses a host with no
  genesis. `Launch::prompt_for` resolves each node's actual engine grants,
  role, fuel and name into the stored prompt base.
- `Fleet::claim` checks and reserves the name and an unbound tenant before
  logs or engine construction. `Claim::occupy(engine)` makes a `Tenancy`;
  `Claim::bind` records the owner's handle. `Tenancy` certifies and drops
  its engine before its claim releases. An armed claim publishes terminal
  `Died` before removing its name/census entry, outside the census lock;
  a bare unarmed claim releases silently and certifies nothing.
- `Agent::assemble` (`agent/build.rs`) adopts through the call's captured
  mailbox only after engine and log exist. A dead parent is `SessionDead`;
  abandoned work is `Abandoned`. A root's life is a fleet-root child; a
  child's life is a child of its parent's captured work. Its engine incarnation
  is a child of life, parallel to work.
- `Fleet::{resolve, live, roots}` derive live addresses from names, sorted
  by id, with roots filtered by role. There is no separate children or roots
  index. `roster::listing` / `summary` project these live handles.
- `Fleet::end` cancels its `DurableRoot`, notifies inboxes, and leaves any
  later root born cancelled. `signals::face` forwards ambient SIGINT to
  roots' exchanges and shutdown to the fleet root.
- `Fleet::census` returns the detached `Census`. `is_empty` / `wait_empty`
  wait for no tenants; `wait_settled(waiter, &Pumping)` waits until every
  tenant but the waiter's is bound and live, then reports accumulated
  teardown failures. The census takes no inbox lock under its own.
- `Agent::drop` ends life first, clears schedules and records `SessionEnded`
  unless panicking; the inbox then reports any owed failure, and the
  `Incarnation` drops last: its `Tenancy` awaits a wire engine's
  `Detach → Ended`, records a missing acknowledgment as a census failure,
  then drops the engine. Its claim publishes any armed terminal cue before
  releasing name and tenant, so a final drain cannot overtake death.
  Owner drop ends descendants; retained addresses never prolong teardown.

`Agent::{root, resume, hatch}` build roots; `/new` and `/branch`
(`spawn::spawn_root`, `hatch_root`) claim then boot fresh engines. Branch
imports context, not shell state. Resume reads every root record lacking
`SessionClosed`, retaining its recorded name; headless resume requires
exactly one live root. Chat, wire and child resumes are refused. The run
lock belongs to the launcher, not an agent.

## Attend, deliberate, invoke

**One owner loop handles every role.**

- `Agent::attend` (`agent/attend.rs`) publishes idle state before `draw`,
  outside its lock. The inbox re-evaluates `park_mode` on every wake,
  refuses dead life or a stopped job, and pops queued live work before
  honouring `Quiesce`. `PARK_POLL` also bounds ancestor-cancel observation.
- Parking is computed: an attended conversing root is `Held`; a replied
  child is `Engaged`; outstanding obligations give `HeldByChildren`; armed
  schedules give `UntilCancelled`; otherwise `Quiesce`. Severance always
  quiesces, while life cancellation is checked by the inbox itself.
- `take_up` runs reads/rewrites or calls `arrive`. A declined claimed-worker
  completion drops its `Opening`. An accepted exchange-opening item
  `open`s the `Opening` minted at pop and cancels its predecessor; a nudge
  continues the existing exchange. No cancellation reset occurs.
- `attempt` snapshots the provider and catches host-side panics around
  `deliberate(exchange, …)`. Public `deliberate` checks recording availability
  on entry and reconciles on return; `attempt` also reconciles caught panics.
  The deliberation appends the prompt before polling
  cancellation: a drawn human line survives a racing gesture in the record,
  without starting provider work under a cancelled exchange.
- `deliberate` (`agent/deliberate.rs`) weighs forced eviction before every
  provider round, streams through `commit::Stream`, admits and commits the
  assistant turn, then answers all requested tool ids. `run_batch` and `invoke` in that module
  dispatch the advertised `Toolset` through `agent/tools.rs` in order.
- Continuing batches call `Inbox::drain` for reads and steering up to the
  first rewrite. A cancelled exchange drains nothing. A staged reply is
  checked after the complete batch and before this drain or reminders.
- `replied` first commits its required quiesce records, then abandons work
  with `Keep`, clears schedules and publishes Ready. `deposit_reply`
  stores the value and pays every taken-up debt as
  `Replied`. A child parks; a headless root returns. Consuming `settle`
  (`agent/spawn.rs`) checks final availability, reports failure through the
  independent fleet sink, retires the inbox and arms its claim's death cue.
  Teardown and terminal publication remain inside census-counted completion.
- A severed engine runs no more calls. `EngineLost` words the cause for
  model and log; headless/Synod end the conversation. Fallible quiesce
  leaves a usable session prompt-admissible; failed mandatory repair or
  a shared writer fault instead ends the affected owner as unavailable.
  Neither a provider request nor ordinary Ready parking follows that fault
  ([[invariants/turn-ends-ready|exchange-ends-ready]]).

## Fork, lease and lifecycle

**The engine makes the snapshot; the owner adopts its recipient.**

- `fork_then_enquire` takes `Shell::bequeath` with the spawn's `SpawnGrant`.
  Identity parks it for `IdentityTransport::adopt_parked`; wire seals it for
  the host's factory. Both scrub handles to opaque placeholders and preserve
  the same serialisable scope, handlers, builtin table and session policy.
- `desk/agents.rs` resolves `model` first, obtains the child engine through
  `fork_engine`, then `Agent::child_of` builds its prompt/log and adopts it.
  Wire construction asks `Factory::engine` for one stream, attaches with
  the parent's `EngineSpec` and seal, and waits before naming the child.
- `spawn_async` announces birth before launching the detached attend thread;
  failed thread creation drops the owner and reports failure. Fuel is
  `parent - 1`, so it bounds depth, not fan-out.
- `AgentLog::fork` imports context for `mnemon`, omitting an unanswered
  tool-call frame. `amnemon` imports none. Both share the shell snapshot
  law; neither flows shell mutations back to the parent.
- Reply lease starts only on a child's deposit, for `AGENT_LEASE_IDLE`
  (one hour), and only if no input is queued. `Fleet::lease_reply` holds the
  address and the `Lease` weakly; accepted input breaks the lease, and
  `lapse` cancels life only while life is live and the lease still holds,
  atomically under the inbox lock. Reading, focus and probes renew nothing;
  roots have no reply lease.
- `/clear` preflights log readiness/availability and rebootability, captures
  a non-destructive inbox boundary, then prepares a fresh engine. A refused
  or failed clear leaves live resources unchanged. After `Cleared` commits,
  it applies that boundary, cancels old work, clears schedules/pins, rebirths
  readings and installs the prepared engine. Later human input survives,
  including a coalesced suffix; the boundary precedes old engine teardown.
- `/rewind` withdraws context, closes the exchange to fence continuations,
  and rebirths nudge state; it does not rewind shell effects. `/evict` is an
  unconditional explicit edit. `/close` ends life, records `SessionClosed`
  and leaves attendance; `/quit` ends the fleet, leaving roots resumable.

## Context, notices and accounting

**The record is durable; readings and reminders belong to one live context.**

- `agent/gauge.rs` owns context-pressure measurement, forced-eviction plans,
  scratch/disk readings and `Gauges`; `latch.rs` owns the shared rung rule.
  `agent/nudge.rs` owns wording, the consecutive repair budget and pin digest.
  Repairs spend a budget; changed standing conditions are told once per edge.
- Forced eviction is a prefix span, never the newest item and never a lone
  residue; no provider call or note is fabricated. Model eviction supplies
  a required note. [[internals/session-record|Session-record]] maps the fold.
- Engine-ready notices carry binding prunes, large-binding warnings and
  worker reaps. Exarch records them; it keeps no second lease/retention clock.
  `shell_eval::arm_session_ledgers` supplies the shared 256-call terms;
  inherited bindings are baseline in a child.
- `/resources` is an owner-thread `Read`: typed transport probes cover
  workers, binding ledger and scratch; local probes cover inbox, log and
  reply leases. The frontend adds only its own accumulator rows. Probes
  mutate nothing and renew no lease.
- `agent/digest.rs` bounds model-visible result sections (value 20 KiB,
  stdout/stderr 10 KiB); `shell_eval/clip.rs` supplies head+tail clipping.
  `Agent::ral` appends `TURN: <id>` outside those caps. Replies stay values,
  fetched through the checked reply boundary rather than clipped prose.
- `net_policy::load` reads exact host policy plus search; only search rides
  `RootConfig` / `Launch`. The host network ledger belongs to netgate, not
  to the agent ([[map/synod|synod]], [[design/egress|egress]]).

[[map/exarch|exarch]], [[map/exarch/frontend|frontend]],
[[map/exarch/provider|provider]], [[map/exarch/shell-eval|shell-eval]],
[[map/exarch/builtins|builtins]], [[map/exarch/policy|policy]],
[[map/core/engine-protocol|engine-protocol]], [[design/residency|residency]].
