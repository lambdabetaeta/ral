---
status: active
generated_at_commit: 1f2056b4d00720b668339430fa3ccc87ee1cd528
verified_at_commit: 1f2056b4d00720b668339430fa3ccc87ee1cd528
verified_at_date: 2026-10-28
anchors: [Agent, AgentHandle, Inbox, Mailbox, Scope, ScopeNode, Scope::id, Job, JobId, JobEnd, SettledConversation::await_job, SettledConversation::run, AgentHandle::stop, Fleet::wait_settled, Membership, Departures, RootEngine, Pool, dispatch_to_report]
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
| Synod Stop / Esc; `/clear`; `reply` | work, replaced by fresh work | `Cancelled` |
| `/clear` | engine incarnation, replaced even if boot fails | `Cancelled` |
| `end`; `/close`; agent cancel; owner drop | life | `Cancelled` |
| idle reply lease lapse | life | `TimedOut` |
| `/quit`; `end_fleet`; SIGTERM / SIGHUP | fleet root | `Cancelled` / `Terminated` |
| process abort | fleet root | `Aborted` |

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
- A normally ended sender's message still arrives: its containing scope
  vouches for what it said. A message from a stopped subtree dies with the
  abandoned work.
- An engine's loose ral workers survive Stop and `reply`; their batches
  survive too. Identity workers retain their birth sink, wire batches use the
  currently installed sink; engine-incarnation provenance gives both the
  same admission. `/clear` cancels and replaces that scope even if boot fails,
  fencing queued and future batches. A successful reboot also terminates the
  old transport before replacing it, even if another holder retains it.
- Wakeups coalesce by never-reused schedule id, nudges replace queued nudges,
  and trailing human posts join with a newline. Source-depth probes remain
  observations, not quotas.

## Work and exact outstanding replies

**Abandonment replaces work, not the agent.** Under the inbox lock it replaces
`Work`, cancels the old scope, drops its outstanding-child map, applies the
queue policy, and notifies.

- `Keep` leaves posts queued for `reply`; `Recall` withdraws human text
  oldest first for Stop; `Discard` drops everything for `/clear`.
  Other posts survive Stop according to their provenance.
- Owner-side abandonment clears schedules after unlocking. Stop clears no
  owner register; schedule callbacks retire entries whose weak mailbox has
  died or whose work is cancelled. Schedule ids never reset.
- `Mailbox::adopt` accepts only the current live work, atomically records
  the child and mints its life beneath that work. Engine/log construction
  precedes adoption outside locks; refusal drops that construction and
  reports `Unborn::Abandoned` while the target life is live, otherwise
  `Unborn::SessionDead`.
- `awaiting[c] = n` means child `c` owes a result answering message `n`, or
  a terminal result, unless this work is abandoned. Adoption starts at zero;
  messaging a direct child publishes a fresh sequence before delivering it.
- Every result carries `answers`. At pop, a reply discharges the child only
  if `answers >= awaiting[c]`; any non-reply outcome discharges it.
  An old queued reply cannot discharge a newer follow-up. Accepted and
  taken-up message sequences are distinct: a reply deposited before taking
  up an accepted follow-up does not answer it or arm its lease. Retirement
  closes admission under that same lock and reports a terminal result if
  input remains unanswered. A dropped child still owing a report posts
  `Failed("ended before reporting")`.
- Busy means exactly `!awaiting.is_empty()`, with no liveness filter.

## Exchanges and jobs

An *exchange* is one item taken up by exarch, including its continuations;
a *job* is Synod's human-opened unit through fleet settlement.

- `draw` tests life and a stopped job, evaluates the park verdict, then pops
  live posts before honouring `Quiesce`. An exchange-opening item mints its
  `opening` under the work in force at pop.
- Accepted arrival cancels the preceding exchange and installs that opening.
  A declined worker completion discards only its opening, preserving the
  current exchange and queued continuation. `/rewind` closes the exchange.
- `deliberate(exchange, …)` appends the prompt before checking cancellation.
  Text drawn just before Stop or Esc is recorded once, quiesced cancelled,
  and starts no provider round. A mid-exchange drain returns nothing under
  cancellation, otherwise hears live deliveries up to the first rewrite.
- `SettledConversation::await_job` opens a `Job` under the root inbox lock
  only when human text is queued. `run(job, sink)` attends it at most once;
  dropping an unattended job closes it. `JobEnd` carries id, stopped verdict
  and outcome; the bus lives across jobs.
- `AgentHandle::stop(id)` acts only on the matching running, unstopped job,
  atomically marks it stopped and abandons with `Recall`. A stale or duplicate
  Stop returns `None` and changes nothing.
- A stopped job pops nothing more. Text sent after Stop stays queued and
  opens the next job. Every `run` waits for ended agents' teardown through
  `Fleet::wait_settled`, inside the output pump, before linearising closing
  against Stop; if Stop wins, it settles again. Only then does it return
  `JobEnd`; the closing folder
  walk and “Stopped.” cannot overtake helpers' last writes or final output.

Synod publishes `JobBegan` / `JobDone` and accepts synchronous `stop_job` and
`recall_queued`. Send remains available while busy; the queue strip shows
waiting input, while human transcript position is recorded uptake. Stop
captures attempt and job at gesture time. Serialized withdrawals restore text
in gesture order ahead of the live draft; only backend `JobDone.stopped`
draws “Stopped.” after in-flight output. No frontend pending-Stop register or
second backend job arbiter is needed.

## Liveness, teardown and lock order

**Live means life is not cancelled; torn down means membership is released.**

- Owner drop cancels life first, clears schedules, reports any owed failure,
  and records `SessionEnded` unless panicking. Engine destruction ends its
  ral workers; `Name` then removes its slot and releases `Membership`.
- Fleet names hold `Claimed` or `Live(AgentHandle)` slots. `claim` takes
  membership before reserving the name, so construction counts too; `bind`
  transfers it to the owner-held `Name`, whose drop removes the slot before
  releasing membership. Live lookups filter life and sort by id; no pruning or
  separate roots index is needed.
- `Fleet::end` cancels the fleet root and notifies inboxes. A later root is
  born cancelled. `wait_empty` observes detached `Departures`; `wait_settled`
  waits until membership count equals one for its attending owner plus
  live handles excluding that owner's identity. The owner is counted exactly
  once, independent of racing cancellation; ended helpers and unfinished
  claims still block settlement. A failed remote teardown refuses settlement.
- `end_fleet` ends the whole launch, drops its owner, then waits through
  detached `Departures`, retaining no launch or factory. Synod recovers the
  machine only after that barrier.
- Lease lapse is atomic with message acceptance under the inbox lock: it
  cancels life only for the same reply timestamp with no newer accepted
  exchange. Read-only observations renew nothing.
- Never hold two inboxes. Inbox may read the schedule registry's leaf lock;
  timer publication releases it before posting. Departures may read the
  fleet names leaf; nothing acquires departures beneath names. Scope polling
  takes no lock.

## Teardown certification

**Socket closure is not proof that a remote engine has stopped writing.**
Protocol version 34 adds the carrier lifecycle exchange `Detach → Ended`.

- Owner teardown sends `Detach` while retaining membership. The guest closes
  dispatch admission, cancels its root, joins its engine thread and verifies
  that every session worker's live-thread ticket has gone before `Ended`.
- Timeout, severance or an undrained roster produces no acknowledgment.
  Fleet settlement records the failure; Synod ends the conversation without
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
7. `/clear` checks `Engine::rebootable` before changing anything. A boot
   failure after that check leaves the old engine with work/queue discarded.
8. Stop and recall are synchronous commands; settlement waits on the driver
   thread. `withdrawals.js` remains the ordering boundary for restoration.

## Where

- `exarch/src/agent.rs`, `agent/{handle,build,engine,fleet}.rs`: owner,
  address, ancestry, construction, destruction and membership.
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
