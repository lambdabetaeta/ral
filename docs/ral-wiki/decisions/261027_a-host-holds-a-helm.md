---
status: "superseded by [[261028_one-scope-tree-one-inbox]]"
verified_at_commit: e6941768
verified_at_date: 2026-10-28
anchors: [Helm, Avatar::helm, Agent::stop, Agent::abandon_delegation, AdmissionHold, Stamp::admit, Stamp::reset, Membership, Departures, SettledConversation::end_fleet, Inbox::await_human, SettledConversation::await_message, SettledConversation::exchange, ExchangeId, Exchanges]
---

# A host holds a helm

Superseded by [[261028_one-scope-tree-one-inbox]]: its scope tree and inbox
replace this mechanism; the queue, Stop semantics and control ladder stand.

**A host may steer an agent without owning its lifetime.** Exarch's public
host boundary is a cloneable *Helm*, a `Weak<Agent>` obtained from its avatar.
The avatar determines consumer lifetime; an upgraded identity is not proof of
an attending consumer. Children retain their parent's identity, but neither
those references nor a host handle may prolong fleet teardown
([[260827_agent-and-avatar]]).

## The boundary

The host queues human input and controls its extent through one boundary.

- `steer` queues a human message for the next tool boundary or exchange;
  an ended agent refuses it with `Ended`.
- `queued` observes waiting prompts and commands; `recall` withdraws queued
  prompts oldest first. Neither operates on the transcript.
- `stop` abandons the current exchange; `end` ends the agent and subtree for
  good. A retired consumer refuses steering and returns empty queue operations;
  `end` can still traverse retained identity to terminate descendants.
- Every method upgrades the weak reference only for its duration. The TUI
  uses the same crate-local `Agent` operations; it never reaches the mailbox
  directly. Interrupt remains crate-local because no external host needs it.

## Interrupt < stop < end

**Stopping an exchange does not end its agent.** The three extents differ:

- *Interrupt* unwinds the current turn; helpers remain alive. This is the
  TUI's Esc.
- *Stop* returns queued prompts, drops and fences the remaining root inbox,
  cancels every descendant and unwinds the root's turn. The agent remains
  available for the next human message.
- *End* cancels the agent and its entire subtree permanently.

**Stop closes admission before delivering cancellation.**

- Under the local inbox mutex, `Mailbox::begin_abandon` drains and fences,
  returns human texts oldest first, publishes cooperative interruption, clears
  outstanding delegation and snapshots direct children.
- An `AdmissionHold` keeps uptake and enrollment closed through descendant
  cancellation and engine interrupt outside locks. Overlapping holds reopen
  admission only after the last delivery; new human text may queue meanwhile.
- A drawn item carries its dequeue stamp. Only an accepted arrival with a
  current generation may reset interruption and remove queued nudges under
  the same boundary. A suppressed completion resets nothing; an old draw
  cannot erase Stop even if it was already being accepted. Already-drawn human
  text refused at either boundary is still recorded exactly once as
  `UserPrompt`, or `Steering` for a mid-exchange draw, without starting provider
  work. Fencing abandoned work must not silently erase human input.
- Child construction retains the originating desk's stamp through final
  enrollment. Generation and cancellation are validated atomically with
  adoption, never refreshed after engine construction. Host-service captures
  retain the invocation's work stamp, including background work.
- Termination closes each node before snapshotting its direct children, then
  recurses outside locks. End, Stop's descendants and `/clear` use this rule;
  an unlocked, precomputed subtree cannot exclude a racing enrollment.

This is `/clear`'s inbox fence: `Agent::forget` performs abandonment and
`Inbox::clear` discards the prompts returned by the same drain operation. Stop does not rebuild the context, clear pins
or schedules, or cancel the root's own shell workers. Their deferred surface
batches are fenced; the workers remain available to poll. An unstamped peer
message already in flight may still land, the same residual as `/clear`.

## Identity, consumer and teardown

**Three observations have different witnesses.**

- Weak roots and children retain traversable identity while descendants exist;
  shutdown walks must not prune a terminated ancestor merely for its token.
- Avatar drop retires its token under admission before any resource teardown,
  including on unwind. Helm refuses a retired consumer even if its identity
  upgrades. Drop does not itself cancel the subtree.
- Fleet emptiness counts Avatar-owned `Membership` guards, released after
  seat/log and fleet/identity references drop. The guard holds only a detached
  `Departures` count/condvar, not the launch or factory. Retained identity Arcs
  and temporary Helm upgrades do not count as outstanding teardown.
- Lock order is inbox/admission before brief fleet, children or status access.
  Never retain an ancestor inbox while acquiring a descendant's. Construction,
  provider work, probes, network writes, cancellation delivery and rejected
  engine destruction remain outside admission/fleet locks.

## One root, human-opened exchanges

**`SettledConversation` owns its root and opens only on human steering.**

- Construction consumes the avatar and binds a conversation-lived bus;
  an exchange cannot accidentally name another root.
- `await_message` waits for any queued `UserSteering` without removing it;
  termination wins even if a prompt is queued. Non-human posts alone,
  including a shell worker's surface batch, never start work.
- `exchange(sink)` attends queued work until the root and busy descendants
  settle with results drained. Ready resident helpers do not hold it open;
  helper signals between exchanges wait for the next drain.
- `end_fleet(self)` intentionally shuts down every root in the shared fleet,
  not merely the owned root's subtree. It retains detached `Departures`,
  cancels the roots, drops the driver/root, then waits for teardown. Synod's
  `Guest::end` uses this boundary before uniquely recovering `MachineFactory`;
  a wait that retained the launch would itself retain the factory.
- Self-scheduling and reset-resume are refused at construction, preserving
  the finite exchange boundary
  ([[260806_exchange-ends-at-fleet-quiescence]]).

## Synod's gestures

Synod's Stop and Esc choose **stop, not interrupt**: helpers must not keep
working against a folder whose closing account is being written.

- Send stays available while busy. Waiting text lives in a queue strip;
  Edit or Up in an empty composer withdraws it oldest first.
- A human prompt enters the transcript where the trunk records its uptake,
  through human-voiced `UserPrompt` or `Steering`, never at submission.
- Each attempt assigns monotonic `ExchangeId`s. `ExchangeBegan`,
  `ExchangeDone` and `stop_exchange(attempt, exchange)` name the same exchange;
  Stop for a completed or different exchange is a no-op, queue untouched.
- Backend idle/running/stopping arbitration reserves accepted Stop before
  out-of-lock delivery. Completion waits for that delivery, so neither it nor
  a subsequent exchange can overtake Stop.
- Stop captures its attempt and exchange ID at gesture time. Per-attempt
  serialized withdrawals preserve Stop/recall gesture order, accumulate an
  overlapping batch, then restore it ahead of the live draft. A delayed request
  cannot retarget a newer exchange or restore into a superseding attempt.
  Nothing starts automatically
  afterwards. Only `ExchangeDone { stopped: true, .. }` draws “Stopped.” after
  in-flight output, regardless of command acknowledgement timing. The frontend's
  pending ID suppresses duplicate clicks; it is not completion evidence.
- The opening baseline settles before the message loop. Queue snapshots are
  read and published under the window slot lock, so a stale snapshot cannot
  replace a fresh one.

## Where

- `exarch/src/agent/helm.rs`: `Helm`; `agent.rs`: `Avatar::helm`,
  `Agent::{stop, abandon_delegation}`.
- `exarch/src/bus/inbox.rs`: `Mailbox::begin_abandon`, `AdmissionHold`,
  `Stamp::{admit, reset}`, `Inbox::await_human`.
- `exarch/src/agent/fleet.rs`: `Membership`, `Departures`, `Fleet::{enrol, wait_empty}`;
  `agent/build.rs`: `Avatar::drop`.
- `synod/src/window/exchange.rs`: `Exchanges`; `synod/src/wire.rs`: `ExchangeId`.
- `exarch/src/headless.rs`: `SettledConversation::{await_message, exchange, end_fleet}`.
- `synod/ui/js/withdrawals.js`: per-attempt serialized withdrawals;
  `conversation.js`: gesture-time Stop capture and live-draft restoration.

[[map/exarch/agent|agent]], [[map/exarch/frontend|frontend]],
[[map/synod|synod]] and [[internals/cancellation|cancellation]] locate the
operations and their host projections.
