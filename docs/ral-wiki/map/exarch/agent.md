---
generated_at_commit: e5a23909
generated_at_date: 2026-10-29
covers_paths: [exarch/src/agent.rs, exarch/src/agent/, exarch/src/host.rs, exarch/src/bus/mailbox.rs, exarch/src/bus/lease.rs, exarch/src/latch.rs, exarch/src/app.rs, exarch/src/boot.rs, exarch/src/signals.rs, exarch/src/schedule.rs, exarch/src/enquiry.rs, exarch/src/prompt.rs, exarch/src/config.rs, exarch/src/net_policy.rs, exarch/src/net_policy/]
---

# Map: exarch / agent

**Every `Agent` attends on its own joined thread; `AgentHandle` is its
address.** The laws are [[decisions/261029_an-agent-is-a-thread|an-agent-is-a-thread]];
roles, snapshots and authority are [[design/agents|agents]]'s.

## Owner, address and loan

- `agent.rs::Agent` owns `AgentMailbox`, `Loanable<Body>`, `Option<Engine>`,
  engine generation, fleet and reserved `Name`, in teardown-safe field order.
  `Body` holds `AgentLog` and `State`: nursery, staged reply, report obligation,
  failures, pins, schedules and readings.
- `agent/handle.rs::AgentHandle(Arc<Shared>)` holds identity, directories,
  start time, fixed role/ancestry, fuel, search, provider handle and
  `AgentAddress`. It retains no engine or attending owner.
- `Role::{Converses, Returns, Child { parent }}` fixes whom the agent answers.
  `Trunk::{Converses, Returns}` chooses a root's role; `Admission` separately
  chooses which inputs may open a job at rest.
- `ProviderHandle::{current, swap}` shares model selection. Each attempt
  snapshots it once; children inherit that selection or a launch-bound role.
- `agent/loan.rs::Loanable::lend` moves the body into `Arc<Loan<Body>>` for
  one foreground dispatch, returning it on success or unwind. `Loan::get`
  refuses after return. Workers and pipeline stages have no desk.
- `agent/shell.rs::Agent::ral` builds `RunHost { desk, apply }` for that call.
  `desk.rs::Call` contains only per-call values and
  `AtomicRefCell<Vec<(DeskAct, Option<String>)>>` for committed acts. The
  desk borrows the body through the loan, not persistent shared registers.

## Mailbox, exchanges and jobs

**`bus/mailbox.rs` owns the queue and published status under one mutex.**

- `AgentMailbox` is the life-ending owner end; `AgentAddress` is cloneable.
  `WeakAgentAddress` weakens their shared allocation directly.
- `Input` is human text, message, child report, departure, wakeup or
  deferred surface batch; a `Command` is queued beside it, in order. `next`
  yields an `Input` as `Take` and a command as `Command`; the mid-exchange
  `drain` yields `Drained` (an input or a read) and holds at a rewrite. The
  queue's `Phase` is `Resting { since, released }` or `Working(job)`.
  `Status` snapshots rest time, job, waiting/busy, latest report, parent
  obligation and exchange clock, stamped by `begin`.
- `AgentMailbox::next` selects admissible input and mints an exchange
  `Lease` under life. Commands run without an exchange. `Agent::admit`
  rejects stale epochs/generations and already-claimed completions before
  `begin` commits `JobOpened`; that commit number names the job.
- With a job open, queued input is taken in order; adjacent human fragments
  coalesce. `drain` hears input through the first rewrite only while the
  exchange is live. Human uptake records before polling cancellation.
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
- Lock order is mailbox → log → channel, or mailbox → schedule registry.
  Reporting holds two mailboxes, descendant before ancestor.
  The owner reads helper status without holding its own mailbox.

## Reports and structured teardown

**`agent/nursery.rs::Nursery` owns every helper handle and join guard.**

- `owes_parent` starts true for a helper and is set by each parent message
  atomically with enqueue. `Agent::report` records `Forensic::Status`, clears
  the local report obligation and schedules, then publishes through the handle.
- Under the helper's lock, `AgentAddress::report` publishes its status, posts
  `Input::Child` iff it owes its parent, then leaves the flag set only if
  parent input remains queued. Human-only dealings with a resting helper
  notify no parent model and open no parent job.
- `Rest::pending` (A10): some unjoined helper owes its parent
  (`Nursery::owing`, a snapshot) or has an ended life (`Nursery::lives`, read
  afresh under the parent's lock, since a life ended through its node posts
  nothing before `Gone`). `Status::working` is a view: parent obligation or
  open job.
- `Agent::admit(Child)` records `Display::SubagentDone` and supplies the model
  notice, unless already joined. `Gone` joins and collects failures without
  a model prompt. `read` reaches the newest own helper by name in the nursery.
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
  thread attended (unlaunched or failed resume roots), runs on the caller and
  returns `LaunchError` with spawn cause and failures. After owner/name drop,
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
- `agent/build.rs` constructs log and engine, binds writer failure and
  transport severance to life, then binds the address to its reserved name.
  Root life is under the fleet root; helper life is under its parent's life.
  `RootEngine::{Identity, Wire}` selects construction; `RootEngine::guest`
  adopts guest control with guest paths and unresolved grants.
- `agent/fleet.rs::Fleet` owns launch settings, the `DurableRoot`, name
  directory and joined root threads. `Name` reserves before construction and
  removes on drop. `Launch::engine` uses the optional identity `Rebirth`.
  `resolve`, `live` and `roots` project live addresses; `is_empty` tests names.
  `is_finished` additionally requires every retained root thread finished;
  it observes without consuming join guards.
- `host.rs::Host::{found, resume, adopt}` launches roots, owns the session bus
  and ambient-signal guard, and exposes handles/feed/usage. `finish`, shared
  by `end` and `Drop`, ends the fleet and joins roots, draining repeatedly
  to include racing `/new` launches. Retained addresses cannot delay joins.
- `/new` and `/branch` launch fresh root engines; branch imports context, not
  shell state. Resume replays open root logs with fresh engines and registers.
  Headless requires one open root; the run lock belongs to the launcher.

## Attend and gestures

- `agent/attend.rs::Agent::run` handles `Take`, `Command`, `Settled` and
  `Ended` for every role. Pending helpers publish `WaitingOnAgents`, otherwise
  `Ready`. Nudges are exchange-local continuations; no nudge enters the queue.
- `agent/deliberate.rs` streams provider output, records the assistant turn
  and executes complete ordered tool batches. Provider/tool effects follow
  their records; a failed append prevents the effect. Reply is harvested
  after its batch only while the exchange remains live, then quiesced and
  reported. Failed mandatory protocol repair ends the session.
- `desk/agents.rs` forks through `fork_engine`, constructs the helper and
  adopts its thread into the nursery. Fuel decreases by one, bounding depth.
  `` `stop `` addresses descendants; `` `dismiss `` ends and joins only an
  own helper, keeping teardown failures.
- `Agent::clear` preflights context and prepares a fresh engine without
  mutation, then dismisses helpers and commits `Cleared`. Commit failure
  retires the prepared engine and ends life. Success transfers the life
  binding, replaces/retires the outgoing engine, advances generation, resets
  registers and drops queued reports/wakeups/batches. Human text, messages
  and commands survive; preparation failure changes nothing.
- `/rewind` withdraws context and rebirths nudges, not shell effects.
  `/close` records `SessionClosed` and ends life; `/quit` ends the fleet.
- A resting helper ends itself under the mailbox lock after `launch.idle`
  with no admissible input. `HELPER_IDLE` is five hours; `--helper-idle`
  overrides it. Views, reads and probes renew nothing.
- `schedule.rs::ScheduleRegistry` timers retain `WeakAgentAddress`, backed
  by `Weak<Shared>`. `fire_at` uses `post_wakeup_with`: queued-id check,
  registry update/retirement and publication are one mailbox transaction.
  Epoch changes invalidate wakeups across clear/report. Conversing schedules
  survive jobs; returning schedules hold a job open until reporting.
- `signals::face` stops current root exchanges on SIGINT and ends the fleet
  on shutdown; cancellation never writes to an engine from the cancelling thread.

## Context and accounting

- `gauge.rs` measures pressure, forced eviction and disk/scratch readings;
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
