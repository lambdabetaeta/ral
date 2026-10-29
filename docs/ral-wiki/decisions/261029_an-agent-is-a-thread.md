---
status: active
generated_at_commit: e5a23909
generated_at_date: 2026-10-29
verified_at_commit: e5a23909
verified_at_date: 2026-10-29
anchors: [Agent, AgentMailbox, AgentAddress, WeakAgentAddress, Admission, JobId, Loanable, Loan, Nursery, Joined, Host, Host::is_finished, LaunchError, Body::retire_engine, AgentAddress::post_wakeup_with]
supersedes: [261028_one-scope-tree-one-inbox, 260806_exchange-ends-at-fleet-quiescence, 260826_reply-parks]
amends: [261027_a-host-holds-a-helm, 261029_a-child-is-a-status]
---

# An agent is a thread

**An agent owns its conversation and shell on one joined thread.** Everything
else is its state or a message to it. Hosts gate admission and observe records;
they do not drive exchanges.

## The laws

- **A1 — One owner.** The attending thread owns the log, engine and state;
  only a foreground shell call may borrow the body (A7).
- **A2 — Scopes nest.** Fleet root contains root lives; a parent's life
  contains its helpers' lives; each life contains its exchanges. Stop cancels
  only an exchange with `Interrupted`; stronger gestures end lives.
  Cancellation never resets.
- **A3 — A life ends whole.** Teardown joins helpers before ending its engine.
  `Joined` joins on drop; field order drops the life-ending mailbox before
  the body's nursery, then the engine and name. This holds on unwind too.
  `Host::end` and `Host::drop` end and join every root; no thread is detached.
- **A4 — Jobs are linear.** A job is an agent's observed busy period, not a
  cancellation scope. Under the mailbox lock, `begin` commits `JobOpened`
  and registers its commit number as `JobId`. Settlement commits
  `JobSettled` and clears the job only with an empty queue and nothing
  pending. Stop takes that same lock, marks the open job stopped, cancels
  its latest exchange. Human gestures additionally recall queued text in the
  same critical section; desk Stop and SIGINT leave it queued. Stale ids do nothing;
  a job without settlement ended with its life.
- **A5 — Reports are messages.** `Replied(v)`, `Failed(reason)` and `Stopped`
  are recorded by the agent and published in its mailbox. A helper starts
  with `owes_parent`; each accepted parent message sets it under the queue
  lock. Reporting posts `Input::Child` under that lock iff it owes its
  parent, then resets the flag to whether parent input remains queued.
  Its thread publishes `Died { failures }` after final owner drop, then posts `Gone` to its parent last.
- **A6 — Admission.** At rest, `Free` admits any input, `Gated` admits any
  after a release (given at rest; ignored while working) consumed by job opening, and `Parent` admits human and
  parent input. Commands, `Child` and `Gone` are quiet: never held and do not
  themselves open jobs at rest. Other inputs retain their place. A resting
  helper refuses peer messages; while working it accepts them.
- **A7 — Loans.** The body lives in the call's host during dispatch and
  returns on return or unwind. Later enquiries answer `no desk`.
  `Loan<Body>` and call-local acts use `AtomicRefCell`; neither is a
  persistent shared agent register.
- **A8 — Teardown.** Ending life reports `Failed` if a report is owed,
  joins helpers, retires the engine and returns failures to the joiner.
  Name removal precedes `Died`; `Gone` follows it. Directory emptiness is not
  producer completion: `Host::is_finished` also requires every retained root
  thread finished before the TUI's final drain/frame. `Host::end` still joins
  and collects failures.
- **A9 — Helpers' rest is bounded.** Under the mailbox lock, `next` ends a
  `Parent`-admitted life with `TimedOut` after its idle interval with no
  admissible input. A racing post either wins or is refused. Default:
  `HELPER_IDLE`, five hours; `--helper-idle` overrides it.
- **A10 — Rest owes nothing.** An unjoined helper is pending while it owes
  its parent or its life has ended. A life can end through its node alone
  (the latched record writer, a severed transport), which posts nothing
  until `Gone`; so `Rest` carries a snapshot of the debts and every unjoined
  helper's life `Scope`, which `next` reads afresh under the parent's lock.
  A settlement either sees the end and waits for `Gone`, or precedes it.
  Human dealings with a resting helper remain quiet to the parent model.
- **A11 — Engines end once.** An agent holds `Option<Engine>`; final teardown
  takes it before retirement, so repeated teardown cannot retire it again.
  Clear retires the outgoing engine, or the prepared engine on commit
  failure. `Body::retire_engine` records an uncertified end once as
  `TeardownFailed` and retains it in failures, propagated through `Teardown`
  to `Host::end`, never re-recorded by joiners. Unwind drops an unended engine;
  the panic is the failure. Failed thread creation tears down the unlaunched
  agent; `LaunchError` returns both the spawn cause and teardown failures.

## Delivery and replacement

**The mailbox is the transaction boundary; the owner keeps the state.**

- A popped input receives an exchange lease under life. Only accepted input
  opens a job; stale wakeups and already-claimed batches open nothing.
  Nudges continue the exchange locally, never through the queue.
- Scheduled firing uses `post_wakeup_with`: under the mailbox lock, inspect
  queued wakeups, lock the registry leaf, validate the entry and epoch,
  coalesce, rearm or retire, and publish. A final firing cannot disappear
  between registry retirement and job settlement. Timers retain
  `WeakAgentAddress(Weak<Shared>)`, not a weak outer address wrapper.
- A conversing agent's schedules survive jobs and can wake it from rest;
  a returning agent waits for armed schedules and clears them on reporting.
- `/clear` prepares without mutation, then dismisses and joins helpers,
  commits `Cleared`, transfers the life binding to the fresh engine and
  retires the outgoing one. It resets pins, readings and schedules, advances
  engine generation, and drops queued reports, wakeups and batches; all human
  text, messages and commands remain. Preparation failure changes nothing;
  commit failure ends life and retires the prepared engine.
- Stop preserves helpers, schedules and loose shell workers. Further input
  and helper reports may continue the same stopped job. TUI restoration
  prepends every recalled prompt to the existing draft even during history
  browsing or non-insert mode; ordinary Up recall remains guarded. Synod's
  serialized withdrawals preserve gesture order and attempt identity.
- `` exarch-agents `stop `` targets a descendant's exchange;
  `` `dismiss `` ends and joins only one's own helper.

**Lock order:** mailbox → log → bus channel; mailbox → schedule registry.
Names are a leaf: clone handles out before notification. Hold at most one
mailbox, except reporting from descendant to ancestor. Read helper status
without holding the parent's mailbox.

## Supersession

- [[261028_one-scope-tree-one-inbox]] is superseded: its monotone tree
  survives as life ⊃ exchange; work scopes, provenance vouching, receipts,
  shared status cells, census and reply lease do not.
- [[260806_exchange-ends-at-fleet-quiescence]] is superseded: jobs are
  recorded busy periods for every agent, with pending helpers joined before
  settlement. Synod gates the next job across its folder walk.
- [[260826_reply-parks]] is superseded: a helper rests and times itself out;
  reporting does not end its helpers.
- [[261027_a-host-holds-a-helm]]'s product boundary is amended to
  **Stop < end**, including Synod: Stop no longer ends helpers.
- [[261029_a-child-is-a-status]] is amended: `ChildStatus` has no `Working`;
  reports are messages posted only while the helper mailbox owes its parent.
  Published mailbox status serves `read` and views.

## Where

- `exarch/src/agent.rs`, `agent/{attend,build,spawn,engine,loan,nursery}.rs`:
  ownership, loop, consumptive retirement, launch and loans.
- `exarch/src/bus/{mailbox,lease}.rs`, `schedule.rs`: admission, observed
  jobs, Stop, reports and scheduled-firing transactions.
- `exarch/src/host.rs`, `headless.rs`, `tui/{app,prompt,tabs}.rs`: joined
  roots and frontend observation, including lossless Stop restoration.
- `synod/src/assistant/{guest,conversation}.rs`, `window/conversation.rs`:
  gated jobs, folder walks and machine recovery after joins.

[[design/agents|agents]], [[internals/cancellation|cancellation]],
[[internals/session-record|session-record]], [[map/exarch/agent|agent]],
[[map/exarch/frontend|frontend]], [[map/synod|synod]].

The terminal verdict includes recording failure through the final `SessionEnded` append. A missing terminal completion is a failure, not a successful teardown.

Headless observes helper panic verdicts received before its root report; it does not continue consuming the bus after that report. Joined teardown failures still reach the process result.
