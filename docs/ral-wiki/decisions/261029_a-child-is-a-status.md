---
status: active
amended_by: "[[261029_an-agent-is-a-thread|an-agent-is-a-thread]]"
generated_at_commit: e5a23909
generated_at_date: 2026-10-29
verified_at_commit: e5a23909
verified_at_date: 2026-10-29
anchors: [ChildStatus, Settlement, Forensic::Status, AgentAddress::report, Nursery::pending, Status::working]
---

# A child is a status

> Amended by [[261029_an-agent-is-a-thread|an-agent-is-a-thread]]: no
> `Working` variant or shared child cell; reports are messages, posted only
> while the helper mailbox owes its parent. Published status lives in that
> mailbox; the historical mechanism below is superseded.

**What a parent can know about a child is its latest state, so a child is a
status, not a sum of owed answers.** This reverses the debt mechanism of
[[261028_one-scope-tree-one-inbox]] and keeps its scope tree and fence.

## Diagnosis

- One reply answered every request a child had taken up with one notice,
  and `` exarch-agents `read `` returned one value: the parent observed one
  state per child.
- Counting it took a token per request, a pool per work, a drop that paid
  `Cancelled` and a withdrawal for refused posts.

## The decision

- `ChildStatus` is `Working | Replied(FOValue) | Failed(String)`, held by a
  `Child` that parent and child share; `Work.children` retains those cells.
  `Child::has_replied` observes the variant without cloning its value.
- **A child is born Working**, enrolled by adoption under its parent's lock.
  **Only its parent makes it Working again, or its own resume:**
  `Mailbox::follow_up` posts, records and sets the status under the child's
  record lock. A resumed returning (headless) root records Working through
  `Inbox::resume`, called from `Agent::reopen`; a conversing root records
  nothing. Peer messages, steering and wakeups do not. The follow-up is the
  private `Post::FollowUp`, built only in `Mailbox::follow_up`; there is no
  `Scoped::FollowUp`, and `Post::voucher` names a scoped post's scope for
  fencing.
- **A reply settles only if no parent follow-up is queued.** A crossing
  reply leaves the child Working and posts no notice; its next reply after
  taking up the follow-up answers both. Other input prevents a lease, not
  settlement.
- **Finish publishes status and notice under the same parent lock.**
  `Mailbox::finish` sets the status and queues
  `Item::Agent { name, settled, elapsed }` together. Busy clears at finish,
  and `draw` pops the notice before honouring `Quiesce`.
- **Busy is some child of the current work Working**, with no liveness
  filter. Life cancellation alone does not clear it; retirement finishes a
  Working child, so the parent cannot quiesce between cancellation and its
  notice. Abandonment replaces the work and forgets its children.
- **Ending finishes only a Working child**, as `Failed(reason)`. The reasons
  live together in `bus/post.rs`: `ENDED_BEFORE_REPORTING` (owner drop),
  `ENDED_BEFORE_ANSWERING` (`settle` with outcome Replied while still
  Working: a follow-up crossed or followed its reply; unreachable in
  production, since attend keeps Replied only while the lease holds) and
  `CANCELLED`; `settle` otherwise uses `AgentOutcome::reason`. Retirement
  preserves a replied child's value and posts nothing for it.
- `read` asks the reader's current work for its newest own child of that
  name, live or ended. It refuses Working and Failed, returns Replied
  without consuming it, and reaches neither grandchildren nor abandoned work.
- A headless root reports to its harness through the same cell, without a
  parent mailbox. Accepted: a headless root running several jobs in one
  conversation would keep `Replied(v1)` during a later job; unreachable today.
- **Construction states the pairing.** `Kin { Trunk(Trunk), Child { parent,
  work } }` determines both the `Role` (`Kin::role`) and the `Lodging { Root,
  Reporting(Child), Under(Mailbox, Child) }` (`Kin::lodging`), so a parent's
  work without a child, or a child without work, cannot be stated;
  `Mailbox::adopt` takes an `Arc<Child>`.

## The status is a fold of the child's log

- `ChildStatus::fold`, test-only, reads Working before the first
  `Forensic::Status`, then the last status recorded: the oracle the live
  status must agree with. `Report::transition` records before showing the
  status, outside every inbox lock.
- The parent's delivery and the child's finish both transition a child, so
  each holds `Child.record` across decision, record and publication: the
  log orders transitions as memory does. Retirement takes the record seam;
  it is idempotent, and a retired child takes no further transition.
- A follow-up is posted before its record, so a refused post records and
  changes nothing. Before retirement, between transitions, a queued follow-up
  implies Working. A failed append still shows the transition: the failure
  has ended life, and the parent must hear it. Retirement's own failed
  `Failed` append shows `Failed(<the recording failure>)`; `settle` retires,
  then checks availability and announces any failure once (`spawn::announce`).
- A Working owner unwinding through a panic shows Failed without recording
  it and skips `SessionEnded`; its log still reads Working. A crash writes
  no terminal status. On normal owner drop, the Failed record precedes
  `SessionEnded`, the tail bookend.
- `ChildStatus::settlement` produces the value-free
  `Settlement::Replied | Failed { reason }` carried by the notice.
  `Agent::arrive` records the parent's uptake as `Display::SubagentDone`;
  that display fact carries no reply value. Model and presentation folds
  ignore `Forensic::Status`.

## Lock order

- No path holds two inbox locks.
- A child's record lock (`Child.record`) precedes any inbox lock and the
  child's log. It is never taken while any inbox lock or status lock is held.
- `Child.status` is a leaf, taken under the parent's inbox lock (busy, finish,
  `child_status`), under the child's own inbox lock (`status()`), or under none.
- Inbox, then the schedule registry's leaf, as today. Scope cancellation and
  polling take no lock.

## Accepted consequences

- A stale reply notice cannot settle a newer follow-up: the status is
  Working, and `read` refuses until its next settling reply. A reply remains
  readable after lease lapse or teardown, until the parent's work is abandoned
  or a newer child of that name is enrolled.
- A child's `/clear` that discards a queued follow-up leaves it Working
  until it next replies or ends.
- Current work retains every child's status and reply value; retirement
  releases the record seam, not the status.

## Where

- `exarch/src/record.rs`: `ChildStatus`, `Settlement`, `Forensic::Status`.
- `exarch/src/bus/mailbox.rs`: `Status::working`, `AgentAddress::report`,
  mailbox publication and parent obligation.
- `exarch/src/agent/nursery.rs`: `Nursery::pending`, retained helper handles
  and joins; `agent/desk/agents.rs`: newest own helper's `read`.
- `exarch/src/agent/{attend,arrival,spawn}.rs`: recording reports, parent
  uptake and terminal reporting/departure.

[[decisions/261029_an-agent-is-a-thread|an-agent-is-a-thread]] states the
current publication and obligation laws; the reasoning above records the
superseded shared-cell mechanism.
