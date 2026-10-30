---
generated_at_commit: 64ecd691
generated_at_date: 2026-10-30
covers_paths: [exarch/src/port.rs, exarch/src/agent.rs, exarch/src/agent/, exarch/src/host.rs, exarch/src/bus/mailbox.rs, exarch/src/bus/transaction.rs, exarch/src/bus/lease.rs, exarch/src/latch.rs, exarch/src/app.rs, exarch/src/boot.rs, exarch/src/signals.rs, exarch/src/schedule.rs, exarch/src/enquiry.rs, exarch/src/prompt.rs, exarch/src/config.rs, exarch/src/net_policy.rs, exarch/src/net_policy/]
---

# Map: exarch / agent

**Every `Agent` attends on its own joined thread; frontends address it by id
through `Port`.** `AgentHandle` is the crate-private address. The laws are
[[decisions/261029_an-agent-is-a-thread|an-agent-is-a-thread]] and
[[decisions/261029_a-frontend-holds-a-port|a-frontend-holds-a-port]];
[[design/agents|agents]] fixes state ownership (F1–F6), parentage, snapshots and
authority.

## Owner, address and loan

- `agent.rs::Agent` owns `AgentMailbox`, `Loanable<Body>`, `Option<Engine>`,
  engine generation, fleet and reserved `Name`, in teardown-safe field order.
  `Body` directly holds `AgentLog` (seam and `Context`), immutable
  `Arc<Provider>` client, nursery, timer registry, pins, readings, staged
  reply and failures. Shared facts are the cell's `Mail`, `Schedules` and
  `Model`, not another owner-state register.
- `agent/handle.rs::AgentHandle(Arc<Shared>)` holds identity, directories,
  start time, fixed `parent: Option<AgentHandle>`, fuel, search and
  `AgentAddress`. It retains no engine or attending owner.
- `None` parent means the human. `AgentHandle::founder` walks parentage to
  the human's child this agent descends from, itself for a human child.
  `returns` is read only from `Mail`; `Admission` independently chooses
  which inputs may open a job at rest.
- `Body.client` is an immutable resource stamped with `Selected`. Birth,
  switch and reopen install that stamp in `Model`; subsequent agent reads
  ask the fold. Each request reads the current client. Helpers inherit it
  or use a launch-bound role. `Status.selected` and `AgentRow.selected`
  project the cell's model; learned windows are agent-local.
- `agent/loan.rs::Loanable::lend` moves the body into `Arc<Loan<Body>>` for
  one foreground dispatch, returning it on success or unwind. `Loan::get`
  refuses after return. Workers and pipeline stages have no desk.
- `agent/shell.rs::Agent::ral` builds `RunHost { desk, apply }` for that call.
  `desk.rs::Call` contains only per-call values and
  `AtomicRefCell<Vec<(DeskAct, Option<String>)>>` for committed acts. The
  desk borrows the body through the loan, not persistent shared registers.

## Mailbox, exchanges and jobs

**`bus/mailbox.rs` owns three folds and pending payloads under one mutex.**

- `AgentMailbox` is the life-ending owner end; `AgentAddress` is cloneable.
  `WeakAgentAddress` weakens their shared allocation directly.
- `Input` carries human text, a parent's opening `Task`, messages, child
  reports, wakeups or deferred surface batches. Commands are posts too; `Gone` is separate join plumbing.
  `Txn::post` commits `Posted` before queuing its payload. Every other
  commit prunes payloads to `Mail::pending`, the equality at every live unlock.
  `next` and `drain` commit `Taken` before handing payloads over. Human runs
  are maximal over posts; `Gone` does not split one.
- `AgentAddress::transact` and `AgentMailbox::transact` expose closure-scoped
  `Txn`, never a guard. `Txn` reads folds and offers post, report, schedule,
  unschedule, fire, model change, narrowing and clear operations, not lines;
  `Txn::narrow` accepts a window only if `Model::narrows` does.
  Poison latches the journal and ends life; debug builds detect re-entry.
  The mail's job determines working/resting; release, Stop and loop clocks
  are cell plumbing. `Status` reads report, debt, job and selected model from
  folds beside live loop observations.
- `AgentMailbox::next` selects admissible input and mints an exchange
  `Lease` under life. Commands run without an exchange. `Agent::admit`
  rejects stale generations and already-claimed completions before
  `begin` commits `JobOpened`; that commit number names the job.
- With a job open, queued input is taken in order; human fragments
  coalesce across intervening `Gone` notices, not across other posts. `drain`
  hears input through the first rewrite only while the exchange is live. Human uptake records before polling cancellation.
- `next` commits `JobSettled` and clears the job under the same lock that
  finds the queue empty, no helper pending and no returning-agent schedule
  armed. A post either joins that job or sees rest.
- `Admission::Free` admits any input at rest. `Gated` requires a release,
  consumed by opening one job. `Parent { idle }` admits only human and parent
  input; peer messages to a resting helper are refused. Commands, `Child`
  and `Gone` are quiet and never held by admission.
- `AgentAddress::stop(job)` marks that open job stopped, cancels its latest
  exchange with `Interrupted` and takes back queued human text under the same
  lock; `stop_current` (desk Stop, SIGINT) stops whatever job is open and
  leaves the queue.
  At rest or for a stale id it does nothing. Helpers and schedules survive;
  later exchanges can run within the same stopped job.
- Lock order is descendant cell → ancestor cell → journal → channel.
  Names and deadline heap are leaves; the timer registry has no lock.
  Only reporting nests cells; no transaction spans a provider request,
  dispatch, join or `Context` access.
  The owner reads helper status without holding its own mailbox.

## Reports and structured teardown

**`agent/nursery.rs::Nursery` owns adopted children and their join guards.**

- `Mail::owes` derives debt from taken-but-unanswered parent input and
  pending parent posts, gated by `returns`. Every mail starts owing nothing.
  A helper owes from its task's post; a returning child of the human owes at
  human post time. `Input::Task` posts `Voice::Peer`, bare text and the
  parent's id; arrival admits it bare, unlike later rendered messages.
  Recall returns only human text, never a helper's task. Withdrawal removes
  unread debt, uptake keeps it.
- `AgentHandle::report` calls `Txn::report(status, notice)`, where `notice` builds the `Input` from a `ChildStatus`: commit
  `Control::Status`, schedule retirements and queued-wakeup withdrawals, then
  post `Input::Child` to the fixed parent iff owed, still under the helper
  lock. After life ends, every pending post is withdrawn. A poisoned helper
  reports `Failed` directly to its parent. A failed append notifies iff the
  pre-append mail owed, leaving the folds unchanged. `AgentAddress::report`
  owns both fallback paths, guarded by `Shared.failure_noticed`:
  at most one `Failed` reaches the parent, however often teardown retries.
  `AgentAddress::try_transact` is `transact` for closures returning
  `Result<_, Unavailable>`. `failure_noticed` exists because a failed `Status`
  append leaves the mail owing: teardown's report would fail again and post a
  second `Failed`.
  `Agent::report` drops its timers.
- `Rest::pending` (A10) snapshots `Nursery::owing`: an unjoined helper is
  pending exactly while `Status.owes` is true. An ended reported helper
  owes nothing, even before `Gone` is joined. `Status::working` is a view:
  parent obligation or open job.
- A12 reminds an owing agent when no worker runs, `Rest` is empty and no
  `Human`, `Task`, `Message` or `Child` is queued. Reading `Rest` before the
  queue prevents a reminder from overtaking a helper's report.
- `Agent::admit` on `Input::Child` records `Display::SubagentDone` and
  supplies the model notice only for an adopted, unjoined helper.
  `Next::Gone` and `Drained::Gone` join and collect failures without a model prompt. `read` reaches the newest own helper by name.
- `Agent::teardown` ends life, reports owed failure, dismisses and joins
  helpers, takes `Option<Engine>` and retires it once; a repeat finds no
  engine and does nothing. `Body::retire_engine` records `TeardownFailed` and
  retains failures for the joiner; joiners propagate, never record them again.
  `EngineSevered` records the loss, read once before intentional engine end.
- `Joined` joins on drop. On unwind, mailbox drop ends life before nursery
  join guards run, then the engine drops and the name leaves the directory.
  Normal `Agent::drop` tears down if no owner did and records `SessionEnded`.
- `Agent::launch` spawns a thread awaiting the agent on a one-shot channel,
  then announces `Born`, queues the first input and hands it over. A failed
  spawn keeps the agent, so `Agent::discard`, the teardown of every agent no
  thread attended (unlaunched or failed resumed children of the human),
  runs on the caller and returns `LaunchError` with spawn cause and failures. After owner/name drop,
  the thread's terminal guard adds any final recording failure to
  `Died { failures }`, then posts `Gone` last. A panic reports failure through
  the surviving address.

## Engines, construction and host

- `agent/engine.rs::Engine` holds either identity transport with optional
  `Rebirth`, or wire transport with `EngineSpec`. `read` makes rejected probes
  terminal; `end` certifies teardown. Wire end requires `Detach → Ended`.
- `Engine::install` stamps deferred batches with generation. `inherit`
  transfers the outgoing transport's life binding to its replacement before
  retiring it. Loose ral workers belong to the engine and survive Stop/reply.
- `agent/build.rs::Agent::born` mints the id, computes `Parent::terms`,
  resolves the prompt and opens `AgentLog::create` with `Opening`. Born and
  reopen share private `hatch` and `assemble`, binding writer failure and
  transport severance to life, then the address to its reserved name.
  `Parent::terms` is the one construction case split: human children take
  launch fuel, search, admission and `returns`, with life under the fleet
  root; helpers take their parent's search and one less fuel, nested life,
  `Parent` admission and `returns: true`.
  `LaunchConfig` supplies launch policy. `RootEngine::{Identity, Wire}`
  selects engine construction; `RootEngine::guest` adopts guest control
  with guest paths and unresolved grants.
- `agent/fleet.rs::Fleet` owns `Launch`, `Arc<Registry>` and
  `human: Mutex<Nursery>`, the human's adopted children and joins.
  `Registry` owns `DurableRoot` and the name directory; `Name` reserves
  before construction and removes on drop through a weak registry.
  `unnamed` reserves the least unused `agent-N`, `N ≥ 1`, under that lock.
  `get`, `resolve`, `live` and `human_children` project live addresses;
  `live` is in id order. `Launch::engine` uses the optional identity `Rebirth`.
- `host.rs::Host::{found, resume, adopt}` launches the human's children,
  owns the session bus and ambient-signal guard, and exposes a port, feed
  and run directory. `Host::children()` reads the human's nursery: every
  adopted child in birth order, joined or not, including `/new` and `/branch`.
  `finish`, shared by `end` and `Drop`, ends the registry root and repeatedly
  calls `join_children` to include racing births. The human receives no
  `Gone`; the frontend observes `Died`. A retained `Port` holds registry and
  usage meter, never the fleet or its engine factory.
- `spawn::human_child` makes `/new` and `/branch` births through
  `Agent::born(Parent::Human, …)` with fresh engines; branch imports context, not
  shell state. `AgentLog::reopen` folds context and control before quarantine,
  quiesces and commits one resume line withdrawing pending posts, settling
  the open job and retiring bookings, with a note naming withdrawn inputs.
  It returns reconciled folds for a fresh cell, preserving only taken debt
  and a same-model narrowing. Engines and working memory are fresh.
  Headless reopens exactly one session; the run lock belongs to the launcher.

## Attend and gestures

- `agent/attend.rs::Agent::run` handles `Take`, `Command`, `Settled`, `Gone`, `Rung` and
  `Ended` for every agent. Pending helpers publish `WaitingOnAgents`, otherwise
  `Ready`. Nudges are exchange-local continuations; no nudge enters the queue.
- `agent/deliberate.rs` streams provider output, records the assistant turn
  and executes complete ordered tool batches. Provider/tool effects follow
  their records; a failed append prevents the effect. Reply is harvested
  after its batch only while the exchange remains live, then quiesced and
  reported. Failed mandatory protocol repair ends the session.
- `desk/agents.rs` checks log availability, forks through `fork_engine`,
  constructs the helper through `Agent::born(Parent::Agent, …)`, and launches
  it with its parent's `Input::Task`. The nursery adopts its thread.
  Fuel decreases by one, bounding depth. `reply` reads `Mail::returns`
  through the address transaction; a conversing agent holds no reply.
  `roster.rs` emits `Spawner::Human` (`` `human ``) or the parent's name and
  restricts the roster to agents sharing the reader's `founder`.
  `` `stop `` addresses descendants; `` `dismiss `` ends and joins only an
  own helper, keeping teardown failures.
- `Agent::clear` preflights context and prepares a fresh engine without
  mutation, then dismisses helpers and commits `Cleared`. Commit failure
  retires the prepared engine and ends life. Success transfers the life
  binding, replaces/retires the outgoing engine, advances generation, resets
  registers, commits queued-report/wakeup/surface withdrawals and schedule
  retirements through `Txn::clear`, then drops timers and joined `Gone`s.
  Human text, messages
  and commands survive; preparation failure changes nothing.
- `agent/command.rs::Agent::switch_model` handles `Command::Model(Pick)`
  at rest or a mid-exchange drain. It builds through `Bureau` with launch
  `max_tokens`, commits `Txn::change_model(built.selected())`, then swaps
  `Body.client`. Build failure changes nothing; append failure ends life.
  The next request uses the new client, even in the same deliberation.
- `/rewind` withdraws context and rebirths nudges, not shell effects.
  `/close` records `SessionClosed` and ends life; `/quit` returns from the TUI
  before `lib::run` ends and joins the host.
- A resting helper ends itself under the mailbox lock after `launch.idle`
  with no admissible input. `HELPER_IDLE` is five hours; `--helper-idle`
  overrides it. Views, reads and probes renew nothing.
- `schedule.rs::ScheduleRegistry` is an unlocked owner resource, one timer
  per booked label. A deadline upgrades `WeakAgentAddress` and only rings;
  the owner calls `fire` in a transaction on `Next::Rung` and before every
  deliberation drain. It ignores superseded ids, checks wall time, commits a
  coalesced wakeup plus booking update/retirement and retimes/drops the timer.
  List and rest read `Schedules`. Clear/report retire before timer drop, so
  stale rings need no epochs. Conversing schedules survive jobs; returning
  schedules hold a job open until reporting.
- `signals::face` reads `Registry::human_children` to stop their current
  exchanges on SIGINT and ends the fleet on shutdown; cancellation never
  writes to an engine from the cancelling thread.

## Context and accounting

- `gauge.rs` measures pressure and forced eviction against the cell's model
  window, plus disk/scratch readings;
  `nudge.rs` owns wording and repair budgets; `latch.rs` owns rung changes.
- `/resources` is an owner-thread read over transport and local mailbox/log
  probes; the frontend contributes only its own accumulators.
- `digest.rs` bounds model-visible results; `Agent::ral` appends `TURN: <id>`
  outside those caps. Replies remain faithful values, checked by `read`.
- `net_policy::load` reads host policy; only search rides agent launch.
  The network ledger belongs to netgate.

[[map/exarch|exarch]], [[map/exarch/frontend|frontend]],
[[map/exarch/provider|provider]], [[map/exarch/shell-eval|shell-eval]],
[[internals/cancellation|cancellation]], [[internals/session-record|session-record]].
