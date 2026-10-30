---
status: active
generated_at_commit: 64ecd691
generated_at_date: 2026-10-30
verified_at_commit: 64ecd691
verified_at_date: 2026-10-30
anchors: [Agent, AgentMailbox, AgentAddress, WeakAgentAddress, Admission, JobId, Loanable, Loan, Nursery, Joined, Host, Port, Registry, LaunchError, Body::retire_engine, AgentAddress::transact, Txn, Mail, Schedules, Model, Agent::born, AgentLog::create, Parent::terms, Host::children]
amended_by: [261029_a-frontend-holds-a-port]
supersedes: [261028_one-scope-tree-one-inbox, 260806_exchange-ends-at-fleet-quiescence, 260826_reply-parks]
amends: [261027_a-host-holds-a-helm, 261029_a-child-is-a-status]
---

# An agent is a thread

**An agent owns its conversation and shell on one joined thread.** Everything
else is its state or a message to it. Hosts gate admission and observe records;
they do not drive exchanges.

## The laws

- **A1 — One owner.** The attending thread owns `Context` and resources
  (engine, client, nursery and timers); the cell owns `Mail`, `Schedules`
  and `Model` under one lock. Only a foreground shell call borrows the
  body (A7).
- **A2 — Scopes nest.** The human's life is the fleet root, containing its
  children's lives; a parent's life
  contains its helpers' lives; each life contains its exchanges. Stop cancels
  only an exchange with `Interrupted`; stronger gestures end lives.
  Cancellation never resets.
- **A3 — A life ends whole.** Teardown joins helpers before ending its engine.
  `Joined` joins on drop; field order drops the life-ending mailbox before
  the body's nursery, then the engine and name. This holds on unwind too.
  `Host::end` and `Host::drop` end and join every child of the human; no
  thread is detached.
- **A4 — Jobs are linear.** A job is an agent's observed busy period, not a
  cancellation scope. Under the cell lock, `begin` commits `Control::JobOpened`;
  its commit number names the job held by `Mail`. Settlement commits
  `JobSettled` and clears the job only with an empty queue and nothing
  pending. Stop takes that same lock, marks the open job stopped, cancels
  its latest exchange. Human gestures additionally recall queued text in the
  same critical section; desk Stop and SIGINT leave it queued. Stale ids do nothing;
  a job without settlement ended with its life.
- **A5 — A parent is owed a reply.** The mail derives debt for every
  returning agent: what it took up and has not answered, or pending posts
  by its parent. Every agent starts owing nothing. A helper owes from its
  task's post and later parent messages; a returning child of the human
  owes from human posts, before admission. Human input to helpers and
  peers add none. `Taken` keeps debt; `Withdrawn` takes unread debt away.
  `Txn::report` commits `Control::Status`, retires schedules and withdraws
  queued wakeups (all pending posts after life ends), then posts `Child`
  iff it owed, under the helper lock. A job settling owing reports `Stopped`
  if stopped, otherwise `Failed`. The helper posts `Died`, then `Gone`,
  last. A parent admits `Child` only from an adopted, unjoined helper.
- **A6 — Admission.** At rest, `Free` admits any input, `Gated` admits any
  after a release (`released` is cell plumbing, set only at rest and
  consumed by job opening; a release while working is ignored), and `Parent`
  admits human and parent input. Commands, `Child` and `Gone` are quiet: never held and do not
  themselves open jobs at rest. Other inputs retain their place. A resting
  helper refuses peer messages; while working it accepts them.
- **A7 — Loans.** The body lives in the call's host during dispatch and
  returns on return or unwind. Later enquiries answer `no desk`.
  `Loan<Body>` and call-local acts use `AtomicRefCell`; neither is a
  persistent shared agent register.
- **A8 — Teardown.** Ending life reports `Failed` if a report is owed,
  joins helpers, retires the engine and returns failures to the joiner.
  Name removal precedes `Died`; `Gone` follows it. Directory emptiness is not
  producer completion: `Host::is_finished` also requires every retained
  human child's thread finished before the TUI's final drain/frame. `Host::end` still joins
  and collects failures.
- **A9 — Helpers' rest is bounded.** Under the mailbox lock, `next` ends a
  `Parent`-admitted life with `TimedOut` after its idle interval with no
  admissible input. A racing post either wins or is refused. Default:
  `HELPER_IDLE`, five hours; `--helper-idle` overrides it.
- **A10 — Rest owes nothing.** An unjoined helper is pending exactly while
  `Mail::owes()` is true. Reporting holds its cell across the status commit
  and parent post, so observing paid debt means the notice was delivered.
  `Rest::pending`, read before the parent's lock, cannot overtake it. A report
  after life ends withdraws every pending post: an ended reported helper
  owes nothing by construction, and `Gone` joins it in or out of a job.
- **A11 — Engines end once.** An agent holds `Option<Engine>`; final teardown
  takes it before retirement, so repeated teardown cannot retire it again.
  Clear retires the outgoing engine, or the prepared engine on commit
  failure. `Body::retire_engine` records an uncertified end once as
  `TeardownFailed` and retains it in failures, propagated through `Teardown`
  to `Host::end`, never re-recorded by joiners. Unwind drops an unended engine;
  the panic is the failure. Failed thread creation tears down the unlaunched
  agent; `LaunchError` returns both the spawn cause and teardown failures.

- **A12 — Reminders.** At the end of an exchange, an owing agent is reminded
  to reply iff no worker runs, `Rest` is empty, and no `Human`, `Task`,
  `Message` or `Child` is queued. `Rest` is read before the queue.

## Delivery and replacement

**The cell is the transaction boundary; the owner keeps the context and resources.**

- A popped input receives an exchange lease under life. Only accepted input
  opens a job; stale-generation or already-claimed batches open nothing.
  Nudges continue the exchange locally, never through the queue.
- A timer retains `WeakAgentAddress(Weak<Shared>)` and only rings its cell,
  recording nothing. The owner answers `Next::Rung` and rings before drains,
  reads `Schedules`, coalesces queued timer ids and commits the fire before
  retiming or dropping its timer. The registry is owner-held and unlocked;
  retiring a booking makes late rings harmless, without epochs.
- A conversing agent's schedules survive jobs and can wake it from rest;
  a returning agent waits for armed schedules and clears them on reporting.
- `/clear` prepares without mutation, then dismisses and joins helpers,
  commits `Cleared`, transfers the life binding to the fresh engine and
  retires the outgoing one. It resets pins, readings and schedules, advances
  engine generation, commits queued-report/wakeup/batch withdrawals and
  booking retirements, then drops their timers; all human
  text, messages and commands remain. Preparation failure changes nothing;
  commit failure ends life and retires the prepared engine.
- Stop preserves helpers, schedules and loose shell workers. Further input
  and helper reports may continue the same stopped job. TUI restoration
  prepends every recalled prompt to the existing draft even during history
  browsing or non-insert mode; ordinary Up recall remains guarded. Synod's
  serialized withdrawals preserve gesture order and attempt identity.
- `` exarch-agents `stop `` targets a descendant's exchange;
  `` `dismiss `` ends and joins only one's own helper.

**Lock order:** descendant cell → ancestor cell → journal → bus channel.
The timer registry has no lock; the deadline heap is a leaf.
Names are a leaf: clone handles out before notification. Hold at most one
mailbox, except reporting from descendant to ancestor. Read helper status
without holding the parent's mailbox.

## Supersession

- [[261028_one-scope-tree-one-inbox]] is superseded: its monotone tree
  survives as life ⊃ exchange; work scopes, provenance vouching, receipts,
  shared status cells, census and reply lease do not.
- [[260806_exchange-ends-at-fleet-quiescence]] is superseded: jobs are
  recorded busy periods for every agent, with helper debts paid before
  settlement. Synod gates the next job across its folder walk.
- [[260826_reply-parks]] is superseded: a helper rests and times itself out;
  reporting does not end its helpers.
- [[261027_a-host-holds-a-helm]]'s product boundary is amended to
  **Stop < end**, including Synod: Stop no longer ends helpers.
- [[261029_a-child-is-a-status]] is amended: `ChildStatus` has no `Working`;
  reports are messages posted only while the helper mailbox owes its parent.
  The mail's recorded status serves `read` and views.

## Amendment — 2026-10-29: a frontend holds a port

[[261029_a-frontend-holds-a-port]] amends the boundary without changing the
thread and join laws. The immutable client is owned by `Body`; frontends
read `AgentRow.selected` from the cell through `Port`, not the client.
Its P5 replaces A8's completion criterion: after a drain to empty, every known
tab for a child of the human must have received `Died`.
`Host::is_finished` is removed; `Host::end` still joins and collects failures. The laws above include the
fold amendment below.

## Amendment — 2026-10-30: an agent is the fold of its log

[[design/agents|agents]] supplies the cell and F1–F6.
A1, A4–A6, A10 and A12 above state their amended rules: no separate debt
flag, published pick, registry epoch or registry lock. Closure transactions
keep poison; a poisoned cell ends life and its helper still reports failure.
`commit_to` appends and steps together. The context remains owner-held,
not locked with the shared folds.

## Amendment — 2026-10-30: one kind of agent

The human has no model, log or engine. Its life is the fleet root and
`Fleet.human: Mutex<Nursery>` owns its children's handles and joins, as an
agent's nursery owns its helpers. `Host::children()` lists every adopted
child in birth order, joined or not, including `/new` and `/branch` births;
end repeatedly drains the nursery to include racing births. The human is
not sent `Gone`; frontends already observe `Died`.

`Agent::born` and `AgentLog::create` are the one birth and log opening.
`Parent::terms` is the one construction case split: the human lends launch
terms; an agent lends search, one less fuel, nested life, `Parent` admission
and `returns: true`. The handle holds only fixed parentage; `returns` is
read from `Mail`. The helper's opening task is its parent's `Peer` post,
with bare text, and debt begins at that post, not birth. A2 and A5 above
include these rules.

## Where

- `exarch/src/agent.rs`, `agent/{attend,build,spawn,engine,loan,nursery}.rs`:
  ownership, loop, consumptive retirement, launch and loans.
- `exarch/src/bus/{mailbox,lease}.rs`, `schedule.rs`: admission, observed
  jobs, Stop, reports and scheduled-firing transactions.
- `exarch/src/{host,port}.rs`, `agent/fleet.rs`, `headless.rs`,
  `tui/{app,prompt,tabs,tui_loop}.rs`: the human's nursery, registry-backed
  ports and observed child deaths, including lossless Stop restoration.
- `synod/src/assistant/{guest,conversation}.rs`, `window/conversation.rs`:
  gated jobs, folder walks and machine recovery after joins.

[[design/agents|agents]], [[internals/cancellation|cancellation]],
[[internals/session-record|session-record]], [[map/exarch/agent|agent]],
[[map/exarch/frontend|frontend]], [[map/synod|synod]].

The terminal verdict includes recording failure through the final `SessionEnded` append. A missing terminal completion is a failure, not a successful teardown.

Headless observes helper panic verdicts received before the returning agent's report; it does not continue consuming the bus after that report. Joined teardown failures still reach the process result.
