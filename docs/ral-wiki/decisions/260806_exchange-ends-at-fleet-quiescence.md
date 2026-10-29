---
status: superseded
superseded_by: "[[261029_an-agent-is-a-thread|an-agent-is-a-thread]]"
verified_at_commit: 0e83863c
verified_at_date: 2026-10-29
anchors: [SettledConversation::await_job, SettledConversation::run, Fleet::wait_settled, AgentHandle::stop, JobEnd, Census, Tenancy]
---


> Superseded by [[261029_an-agent-is-a-thread|an-agent-is-a-thread]].
# Synod's job ends at fleet quiescence

> Amended by [[261029_a-child-is-a-status|a-child-is-a-status]]: the
> obligation ledger is the current work's child statuses; a child is
> outstanding while Working.

**Synod's closing folder account follows fleet settlement, never merely the
trunk's first quiet moment.** A *job* opens on human input and ends when the
trunk has no more work and its outstanding child results have been heard, or
when Stop has ended the delegated work and its helpers have torn down.

The original decision made Synod a caller of the wire-side spawn protocol,
with `SPAWN_FUEL` (3) rather than a depth-zero trunk. Delegation is universal,
bounded by depth rather than fan-out, with no engine-carrier exception
([[design/agents|agents]]). Exarch's TUI may converse while helpers work;
Synod must not report a stopped helper's folder writes before that helper
has finished tearing down.

## The law

**Ordinary completion measures outstanding work, not resident agents.**

- The trunk is quiescent: nothing remains for it to do in this job.
- Every child obligation has replied or ended, and its result has been
  drained and announced. The work's receipt pool is the exact obligation
  ledger: one `Owed` per adoption and per follow-up.
- Replied helpers remain parked under their follow-up lease. Their presence
  is not busy work and does not hold the job open
  ([[decisions/260826_reply-parks|reply-parks]]).

`SettledConversation` owns one root and a conversation-lived bus.
`await_job` opens on queued human text only; `run(job, sink)` drives the
ordinary attend loop until the root's outstanding work settles. A worker
surface batch alone cannot open a job; signals between jobs wait for the
next drain. The constructor refuses self-scheduling and reset-resume, so a
future wakeup cannot silently extend this boundary.

The park verdict is `HeldByChildren` while obligations remain, not the
TUI's human-attached `Held`. `WaitingOnAgents` names that wait honestly.
A child with a reply parks `Engaged`; Synod's conversing trunk does not.
Returning children must reply or fail after bounded repair nudges. Fuel
bounds depth, not round-trips: mutually sustaining helper messages remain
an open nontermination risk, not a claim this law resolves.

## Stop amendment, 2026-10-28

**A stopped job closes only after ended helpers have torn down.**
[[261028_one-scope-tree-one-inbox]] names the mechanism and retains the
host control ladder, **interrupt < stop < end**.

- Synod's Stop and Esc call `AgentHandle::stop(JobId)`. Under the root inbox
  lock, a matching running job is marked stopped and its work is abandoned.
  Queued human text returns oldest first; other posts live or die by scope
  provenance. The root survives; its loose ral workers remain engine-owned.
- A stopped job takes up nothing new. Text sent afterwards stays queued for
  the next job. A stale or duplicate Stop changes neither queue nor work.
- `SettledConversation::run` waits through `Fleet::wait_settled` before
  closing a stopped job: membership count equals live-agent count, so no
  ended helper is still tearing down. Only then may the closing walk run.
- `JobEnd.stopped` is the backend verdict; Synod's `JobDone` carries it to
  the window, which alone draws “Stopped.” after in-flight output.
- Factory, attach, probe and protocol-write waits remain bounded by their
  own deadlines. A stopped job waits for such a helper's teardown rather
  than claiming an instantaneous barrier.

`Conversation::run` guards guest work with the settled opening baseline,
then takes the closing walk even after failure. Reports are cumulative
metadata accounts against that baseline; a failed walk is unavailable data,
not an empty change set. The account promises no restoration operation.

The count-equals-live predicate has an unresolved boundary: if the attending
root ends during this wait, it retains the very membership settlement waits
to release. Stop followed by shutdown or restart can therefore block the
driver indefinitely
([[261028_one-scope-tree-one-inbox#Unresolved boundaries|unresolved boundaries]]).

## Settlement is not shutdown

**Live, busy and torn down answer different questions.**

- Life cancellation determines whether an agent is live; the work's
  obligation ledger determines whether it is busy.
- `Fleet::is_empty` is true once every claim and `Tenancy` is released,
  after engine and log teardown, not retained address references.
- `SettledConversation::end_fleet` ends every root in the shared launch,
  drops the driver/root, then waits on the detached `Census`, retaining no
  launch or factory. Synod recovers its machine factory only afterwards.
- Owner drop ends its subtree, so retaining a parent's address cannot
  leave descendants working without their consumer.

## Where

- `exarch/src/headless.rs`: `SettledConversation::{await_job, run, end_fleet}`,
  `JobEnd`.
- `exarch/src/bus/inbox.rs`: `Job`, `JobId`, child statuses, addressed Stop.
- `exarch/src/agent/fleet.rs`: `Fleet::wait_settled`, `Census`, `Tenancy`.
- `synod/src/assistant/conversation.rs`: `Conversation::run`, closing walk.

[[design/engine-protocol|engine-protocol]], [[design/agents|agents]],
[[map/synod|synod]], [[map/exarch/agent|agent]].
