---
status: active
generated_at_commit: eb4a62f0
verified_at_commit: eb4a62f0
verified_at_date: 2026-10-28
anchors: [Agent, AgentHandle, Inbox, Mailbox, Scope, ScopeNode, Scope::id, Job, JobId, JobEnd, SettledConversation::await_job, SettledConversation::run, AgentHandle::stop, Fleet::wait_settled, Posts, Owed, Census, Claim, Tenancy, Incarnation, Opening, Ticket, RootEngine, Pool, dispatch_to_report, Unavailable, Emitter::bind, Agent::conclude, Tenancy::discard]
---

# One scope tree, one inbox

**An agent has one owner, one inbox, and one monotone cancellation tree.**
This supersedes [[261027_a-host-holds-a-helm]]'s mechanism and retains its
product boundary: a host steers without owning the attending agent, Synod
queues human input while busy, and **interrupt < stop < end** names three
nested extents rather than three interpretations of a mutable token.

## Diagnosis

Shared registers made correctness depend on remembering the same checks.

- An inbox generation, stopping count and admission holds distributed one
  abandonment decision across enqueue, dequeue, reset and adoption.
- One resettable cancellation token represented interruption, abandonment
  and death; a late reset could erase a stronger gesture.
- Synod reconstructed host-level work identity separately from exarch.
- Identity retention, consumer retirement and teardown each acquired their
  own test for whether an agent was alive.

The replacement makes **scope a value**: cancellation joins over ancestors,
never resets, and a child of a cancelled scope is born cancelled.

## Nested work and an engine incarnation under one fleet root

Exarch uses ral's `Scope` / `ScopeNode`, including for provider requests.

```text
fleet root (DurableRoot)
└── life
    ├── engine incarnation
    └── work
        ├── exchange, including its nudge continuations
        └── each adopted child's life
            └── …
```

| gesture | scope cancelled | cause |
|---|---|---|
| TUI Esc / Ctrl-C; SIGINT forwarded to roots | exchange, including an opening drawn item | `Interrupted` |
| Synod Stop / Esc; committed `/clear`; `reply` | work, replaced by fresh work | `Cancelled` |
| committed `/clear` | engine incarnation, replaced by its prepared successor | `Cancelled` |
| `end`; `/close`; agent cancel; owner drop | life | `Cancelled` |
| first failed record append | life | `Cancelled` |
| idle reply lease lapse | life | `TimedOut` |
| `/quit`; `end_fleet`; SIGTERM / SIGHUP | fleet root | `Cancelled` / `Terminated` |
| process abort | fleet root | `Aborted` |

- An engine incarnation is owned by `Incarnation`, which owns the tenancy
  and so the engine. Clear prepares a successor without changing that scope;
  only committed `Cleared` permits fencing it, installing the prepared engine
  and certifying the outgoing one. Failed preparation preserves the old scope.
- An interrupt preserves helpers. Abandoning work ends every adopted
  descendant and its current exchange, but preserves the agent.
- Ending life ends its subtree. Dropping the owner also ends life; there
  are no surviving descendants whose consumer has disappeared.
- Parked owners poll life every `PARK_POLL`; provider requests poll their
  exchange. Own end, Stop and posts notify the inbox condvar.
- Factory, attach, probe and protocol-write waits have their own deadlines,
  not scope-driven interruption. Cancelled work refuses what follows them.

**Cancellation never pushes into an engine from a cancelling thread.**
`dispatch_to_report(..., Some(exchange))` owns the strike for its own
`DispatchId`, before dispatch and after each nonterminal event or `POLL_SLICE` timeout,
only when the observed cause rises. Identity dispatches mint their node under
that exchange; wire dispatches receive addressed `Control` strikes.
`Control::Strike(id, cause)` preserves every cause and affects only that
dispatch, never the engine root. A pre-dispatch strike foretells cancellation,
so no effect starts in a dead exchange. Engine termination also strikes a current dispatch parented under
an external scope.

## Owner and address

**`Agent` owns the state; `AgentHandle` is its address, not its lifetime.**

- The attending thread alone owns the log, engine, reply staging cell, pins,
  schedules and readings. `Engine` names its ral engine under either carrier;
  `RootEngine` selects root construction. The provider runtime/cache is `Pool`.
- `AgentHandle` shares immutable identity, fixed role and ancestry, a
  hot-swappable provider handle, and a life-scoped mailbox. It retains no
  owner or engine connection.
- A child's `Role::Child { parent }` holds its parent's handle strongly.
  Descendants are the fleet's live handles whose fixed ancestry contains the
  reader; there is no mutable children list.
- Desks run on the owner's thread, inline for identity and in the wire
  dispatch loop. Workers and pipeline stages have no desk.
- `HostServices` captures plain per-call values, including one work mailbox.
  `LogCell` and `ReplyCell` remain single-thread cells; reentrancy is a panic,
  not a reason to introduce another mutex or borrowed desk lifetime.

## One inbox and one fence

**Every cross-thread delivery enters through one inbox mutex and condvar.**
`Inbox` is the non-cloneable owner end; `Mailbox` carries both the destination
and the scope vouching for a post. The inbox belongs in `bus`, below
`schedule`, `shell_eval` and `agent`.

| post | vouching scope |
|---|---|
| child result | parent's work captured at adoption |
| deferred worker surface batch | engine incarnation, a child of life parallel to work |
| scheduled wakeup | work in which it was armed |
| peer message | scope under which the sender's life hangs |
| nudge | exchange it continues |

- The private `Post` sum distinguishes unfenced human text and commands from
  scoped deliveries. **A scoped post is taken up only if its vouching scope
  is live at pop, under the inbox lock.** Cancellation is monotone, so one
  fence suffices. Removing dead posts re-coalesces adjacent human text;
  publication also refuses a dead target life or vouching scope.
- **Queued posts are observed only through `Posts`, and every observation
  fences first.** Pop, emptiness, recall, depths, texts and the wakeup,
  human and input probes all drop dead provenance before answering, so no
  predicate mixes live and dead posts. `Posts::push` alone never fences.
- Lock order: descendant inbox, then ancestor inbox; inbox, then the
  schedule registry's leaf. There are no reverse edges. An `Owed` dropped
  under its child's lock (by a provenance fence or clear boundary) pays into its
  parent through `deliver` and `push`, which never fences, so a payment drops
  no further `Owed` and acquisitions strictly climb the tree. A `Result`
  carries receipts, which take no lock when dropped. Scope cancellation and
  polling take no inbox lock.
- A normally ended sender's message still arrives: its containing scope
  vouches for what it said. A message from a stopped subtree dies with the
  abandoned work.
- An engine's loose ral workers survive Stop and `reply`; their batches
  survive too. Identity workers retain their birth sink, wire batches use the
  currently installed sink; engine-incarnation provenance gives both the
  same admission. A committed `/clear` cancels and replaces that scope,
  fencing queued and future batches. Failed preparation leaves it untouched;
  a failed commit ends life without replacing the incarnation. Installation
  certifies the outgoing transport even if another holder retains it.
- Wakeups coalesce by never-reused schedule id, nudges replace queued nudges,
  and adjacent human fragments are observed as one newline-joined prompt.
  Private arrival ordinals let clear preserve a post-boundary suffix through
  that coalescing. A rewrite held by the mid-exchange drain stays in place
  with its ordinal; provenance remains exclusively scope-vouched. Source-depth
  probes remain observations, not quotas.

## Work and exact outstanding replies

**Abandonment replaces work, not the agent.** Under the inbox lock it replaces
`Work`, cancels the old scope, starts a fresh receipt pool so the old work's
debts no longer count, applies the queue policy, and notifies.

- `Keep` leaves posts queued for `reply`; `Recall` withdraws human text
  oldest first for Stop. Other posts survive Stop according to provenance.
  Clear captures a non-destructive queue boundary before preparing an engine;
  after `Cleared` commits, it abandons work and drops only old-boundary posts.
  Later human input survives preparation, publication and outgoing teardown.
- Owner-side abandonment clears schedules after unlocking. Stop clears no
  owner register; schedule callbacks retire entries whose weak mailbox has
  died or whose work is cancelled. Schedule ids never reset.
- `Mailbox::adopt` accepts only the current live work, atomically mints the
  child's life beneath that work and its first debt. Engine/log construction
  precedes adoption outside locks; refusal drops that construction and
  reports `Unborn::Abandoned` while the target life is live, otherwise
  `Unborn::SessionDead`.
- **Every promised answer is an owned `Owed`.** Adoption mints one debt, and
  so does each follow-up from a parent to its direct child, before delivery
  and never under both inboxes. A debt holds the child's shared `Report`
  (destination, name, birth) and a receipt of the parent's current work. The
  follow-up's debt travels with its message and is taken up with it.
- Paying consumes the debt: a reply moves every debt the child has taken up
  into one `Result` whose receipts enter the parent's queue, and they are
  released when that result is popped. A refused post of any kind withdraws
  its debt silently inside `deliver`; a debt owes only to a live work.
- **Busy means receipts are outstanding for the current work**, with no
  liveness filter and no per-child map. An old queued reply therefore cannot
  discharge a newer follow-up: each debt is its own receipt.
- The default payment is `Cancelled`, made by dropping an `Owed`. An owner's
  end pays `Failed("ended before reporting")`, a dirty retirement pays
  `Cancelled`, and any other outcome pays as given. Retirement closes
  admission under the inbox lock and collects debts still held or queued.
- A reply deposited before taking up an accepted follow-up neither answers it
  nor arms a lease. The lease is a `Weak` whose only strong owner is the
  inbox: newer accepted input breaks it, and a deposit refuses it while such
  input is queued. Lapse cancels life only if life is still live and the
  lease still upgrades, so it cannot outlive a follow-up. Only a child's
  reply is leased; a headless root's reply settles its job however much
  input is queued behind it.

## Exchanges and jobs

An *exchange* is one item taken up by exarch, including its continuations;
a *job* is Synod's human-opened unit through fleet settlement.

- `draw` tests life and a stopped job, evaluates the park verdict, then pops
  live posts before honouring `Quiesce`. An exchange-opening item mints its
  `opening` under the work in force at pop, as an owned `Opening`.
- Accepted arrival consumes the `Opening` with `open`, cancelling the preceding
  exchange and installing it. A declined worker completion drops it, which
  discards only that opening and preserves the current exchange and queued
  continuation. `/rewind` closes the exchange.
- `deliberate(exchange, …)` appends the prompt before checking cancellation.
  Text drawn just before Stop or Esc is recorded once, quiesced cancelled,
  and starts no provider round. A mid-exchange drain returns nothing under
  cancellation, otherwise hears live deliveries up to the first rewrite.
- `SettledConversation::await_job` opens a `Job` under the root inbox lock
  only when human text is queued. The job holds a `Ticket`, its claim on the
  inbox; `run(job, sink)` attends it at most once, and dropping an unattended
  job closes it. A stopped job stays open, held by its ticket, until dropped. `JobEnd` carries id, stopped verdict
  and outcome; the bus lives across jobs.
- `AgentHandle::stop(id)` acts only on the matching running, unstopped job,
  atomically marks it stopped and abandons with `Recall`. A stale or duplicate
  Stop returns `None` and changes nothing.
- A stopped job pops nothing more. Text sent after Stop stays queued and
  opens the next job. Every `run` waits for ended agents' teardown through
  `Fleet::wait_settled`, inside the output pump, before linearising closing
  against Stop; if Stop wins, it settles again. The closing folder walk and
  “Stopped.” cannot overtake helpers' last writes or final output.
- Both pumped termini — the headless `run` and `SettledConversation::run` —
  conclude after their pump. An unavailable writer is announced in a second
  pump however attendance ended; the settled conversation also settles the
  helpers it ended there. The failure overrides the attended outcome, as at
  every terminus. That census report supersedes the first.
  `JobEnd.outcome` joins failures one per line. A settlement failure severs
  once, after the pumps.

Synod publishes `JobBegan` / `JobDone` and accepts synchronous `stop_job` and
`recall_queued`. Send remains available while busy; the queue strip shows
waiting input, while human transcript position is recorded uptake. Stop
captures attempt and job at gesture time. Serialized withdrawals restore text
in gesture order ahead of the live draft; only backend `JobDone.stopped`
draws “Stopped.” after in-flight output. No frontend pending-Stop register or
second backend job arbiter is needed.

## Liveness, teardown and lock order

**Live means life is not cancelled; torn down means the tenancy is released.**

- Owner drop cancels life first, clears schedules and records `SessionEnded`
  unless panicking; the inbox's own drop reports any owed failure. Fields then
  drop in order: the engine's owner is its `Tenancy`, whose drop certifies and
  ends and drops the engine before its `Claim` releases the name and census
  entry. An armed claim publishes final `Died` before that release, outside
  the census lock. A recording failure ends life when the writer latches it.
  The owner's terminus (`settle`, the headless run, `SettledConversation::run`)
  announces it once as `Transient::Fault` on the fleet sink, which the failed
  writer cannot block, then retires; parent retirement precedes the cue.
  The last owner cannot empty the census ahead of its death cue.
- The `Census` holds, under one lock, the name table, the `tenants` map and
  the teardown failures. `Fleet::claim` reserves the name and an unbound
  tenant, so construction counts. `Claim::bind` records the owner's handle;
  `occupy` pairs the claim with an engine as a `Tenancy`. A bare claim, dropped
  before it is occupied or bound, releases silently: nothing was booted that
  needs certifying. Live lookups filter life and sort by id; no pruning or
  separate roots index is needed.
- `Fleet::end` cancels the fleet root and notifies inboxes. A later root is
  born cancelled. `wait_empty` waits on the detached `Census` for no tenants.
  `wait_settled(waiter, &Pumping)` is one predicate over `tenants` under that
  lock: every tenant but the waiter's is bound and live. It is polled, since
  ancestor cancellation does not notify, and returns recorded failures joined
  one per line.
  The attending owner is excluded by its tenant id, independent of racing
  cancellation; ended helpers and unfinished claims still block settlement.
  The `Pumping` witness proves the caller is inside the output pump.
- `end_fleet` ends the whole launch, drops its owner, then waits on the
  detached `Census`, retaining no launch or factory. Synod recovers the
  machine only after that barrier.
- Lease lapse is atomic with message acceptance under the inbox lock: it
  cancels life only while the lease is unbroken by newer accepted input.
  Read-only observations renew nothing.
- Never hold two inboxes except descendant before ancestor, as above. Inbox
  may read the schedule registry's leaf lock; timer publication releases it
  before posting. The census takes no inbox lock under its own: handles are
  cloned out before notifying, and a released tenant drops after unlocking.

## Teardown certification

**Socket closure is not proof that a remote engine has stopped writing.**
Protocol version 34 adds the carrier lifecycle exchange `Detach → Ended`.

- Owner teardown sends `Detach` while retaining its tenancy. The guest closes
  dispatch admission, cancels its root, joins its engine thread and verifies
  that every session worker's live-thread ticket has gone before `Ended`.
- Timeout, severance or an undrained roster produces no acknowledgment.
  **Every outgoing, final and discarded prepared engine is certified**:
  a `Tenancy` certifies on drop and replacement by `Tenancy::install`;
  `Tenancy::discard` certifies the unused engine when a `/clear` commit fails.
  Failures accumulate in the census, so a later clean teardown never absolves
  an earlier one. Settlement reports them; Synod ends the conversation without
  a closing folder scan. EOF alone never authorises that scan.
- Output pumping spans this barrier, including helpers' final facts and
  transients. Baseline failure is terminal too, so queued input cannot reopen
  the failed job in a loop.
- Loose workers of a surviving engine still outlive Stop by policy; this
  barrier certifies ended helpers, not quiescence of surviving engines.

## Accepted choices

D1–D8 fix the policy as well as the mechanism.

1. Pull cancellation uses ral scopes; ancestor cancellation may take one
   park poll to be noticed. No exarch token or push hooks.
2. Synod calls its unit a job; exarch retains exchange for item-level work.
3. Owner drop ends the subtree; retained addresses cannot create orphans.
4. Schedules belong to current work; `reply` and `/clear` clear them,
   while Synod refuses them outright.
5. Messages are vouched by the sender's containing scope, preserving words
   from a normally ended sender but not an abandoned tree.
6. Stop recalls human text; other deliveries live or die by provenance.
   Engine workers are not cancelled by Stop.
7. `/clear` preflights recording/readiness and prepares a rebootable engine
   before mutation. Only committed `Cleared` permits applying the captured
   queue boundary and installing it. Failed preparation preserves live
   bindings, work, incarnation, queued input and owner registers. A failed
   commit is a recording failure: it ends the session and leaves its engine,
   record and registers as they were.
8. Stop and recall are synchronous commands; settlement waits on the driver
   thread. `withdrawals.js` remains the ordering boundary for restoration.

## Where

- `exarch/src/agent.rs`, `agent/{handle,build,engine,fleet}.rs`: owner,
  address, ancestry, construction, destruction and the tenancy census.
- `exarch/src/bus/inbox.rs`: provenance, fence, work, adoption, exchanges,
  job opening/closing and addressed Stop; `bus/post.rs`: delivery payloads.
- `exarch/src/agent/{attend,deliberate,shell,desk}.rs`, `schedule.rs`:
  owner loop, explicit exchange scope and captured work mailboxes.
- `core/src/carrier.rs`, `carrier/identity.rs`, `engine.rs`: dispatch-local
  strikes, scope parenting and engine termination.
- `exarch/src/headless.rs`: `SettledConversation`, `JobEnd` and settlement.
- `synod/src/assistant/{guest,conversation}.rs`, `window/conversation.rs`,
  `wire.rs`: job driver, report barrier, `stop_job` and DTOs.
- `synod/ui/js/{attempt,conversation,ipc,withdrawals}.js`: job projection,
  gesture-time addressing and serialized restoration.

[[design/agents|agents]], [[internals/cancellation|cancellation]],
[[map/exarch/agent|agent]], [[map/exarch/frontend|frontend]],
[[map/synod|synod]].
