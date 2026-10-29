# Agents: one thread, one life, one conversation

**An agent owns its conversation and shell on one joined thread.** Its
address admits messages and publishes status, never retaining the engine or
attending owner. A fleet is a forest of lives, owned through root and helper
join guards ([[decisions/261029_an-agent-is-a-thread|an-agent-is-a-thread]]).

## Role and admission are independent

- `Converses` answers in conversation; `Returns` owes its caller a value;
  `Child { parent }` answers its parent and can rest for follow-ups.
  `Trunk::{Converses, Returns}` chooses how roots answer.
- `Free` admits any input at rest. `Gated` consumes one host release per
  job. `Parent` admits the human and parent; a resting helper refuses peers,
  while a working helper accepts anyone. Quiet commands and child reports
  or departures are never held.
- A helper ends after resting with no admissible input for its idle interval:
  five hours by default, configurable through `--helper-idle`. Reading,
  focus and probes do not renew it. Roots have no idle timeout.

## Stop < end

**Cancellation has two extents: life contains exchange.**

```text
fleet root
└── root life
    ├── exchange and local nudge continuations
    └── helper life
        ├── exchange
        └── helper life …
```

- An exchange takes up one input through its nudge continuations. Stop —
  Esc, Ctrl-C, Synod Stop, SIGINT or the desk's `` `stop `` — cancels only
  that exchange with `Interrupted`. Human gestures recall queued text;
  the desk and SIGINT leave it queued.
- Helpers, schedules and loose shell workers survive Stop. Helpers can
  report and wake the agent again within the same busy period.
- End, `/close`, dismissal, writer failure or engine severance ends life.
  Teardown joins helpers before ending its engine; owner drop preserves that
  ordering on unwind. A retained address cannot leave an orphan thread.
- `` `dismiss `` ends and joins one's own helper; `` `stop `` may target a
  descendant. `/clear` dismisses helpers only after engine preparation succeeds.
- Provider waits poll exchange; ral dispatchers strike only their own id.
  Cancelling threads do not call into engines. Factory, attach, probe and
  protocol writes keep their own deadlines. Cancellation never resets.

## Busy periods are observed

**A job is a recorded busy period, not a cancellation scope.**

- Accepted input at rest opens `JobOpened`, whose commit number names it.
  Declined stale input opens nothing. Mid-job input opens no second job.
- `JobSettled` is committed under the queue lock that finds nothing queued
  and no pending helpers. An unjoined helper is pending while it owes its
  parent or its life has ended; the parent reads each helper's life afresh
  under its own lock, so an end seen before settlement is joined first.
- Stop shares this lock and marks the matching job stopped; human gestures recall text;
  stale ids do nothing. Further input may run before that same job settles.
- `Host` launches roots and owns their feed, then ends and joins all of
  them on end or drop. Frontends observe; they do not drive agents.
- Synod gates each job across its baseline and closing folder walk. A
  release allows human text, wakeups, worker batches or messages to open a
  job. Scheduling and reset-resume remain disabled by Synod policy.
- TUI Stop restoration prepends recalled text to the live draft without
  Up's mode/history guards. Synod serializes withdrawals by gesture and
  attempt. Human transcript position is recorded uptake, not submission.

## Reports belong to whoever asked

**A helper's published status and its obligation are different facts.**

- A returning agent owes a report after taking up human or parent input,
  until reply, rest or end. Reports are `Replied(FOValue)`, `Failed(reason)`
  or `Stopped`, recorded in its own log and published in its mailbox.
- A helper's mailbox starts owing its parent. Parent enqueue sets that flag
  atomically; reporting posts `Child` to the parent under the helper lock,
  then retains the flag only if another parent message remains queued.
- The parent cannot rest between seeing the obligation and hearing the
  report. `Gone`, the helper thread's last post, joins without a model prompt.
- Human-only dealings with a resting helper are quiet to its parent: views
  show its job/report, but the parent's model is not notified and no parent
  job opens. A view's working predicate is obligation or open job.
- `read` asks the nursery for the newest own helper by name, including after
  teardown; it refuses while live and working and returns the faithful reply value.
- The body is owned plainly between shell calls and lent only for foreground
  dispatch. The loan returns on unwind too; a later enquiry answers `no desk`.
  Call-local acts use `AtomicRefCell`, not a persistent shared agent cell.

## Roots, replacement and teardown

**Roots are sessions with fresh engines; helpers are shell snapshots.**

- `/new` opens blank context; `/branch` imports context, not shell state.
  `/close` records permanent closure; `/quit` leaves roots resumable.
  Resume folds open root logs with fresh shell state, not restored workers,
  pins or schedules. Headless requires one open root.
- Names are reserved before construction and removed after owner teardown,
  before `Died`. `Gone` follows death. Retained addresses retain no engine.
- `/clear` preflights context and prepares an eligible identity-root engine
  before mutation. Failure changes nothing. Then it joins helpers, commits
  `Cleared`, transfers the life binding, replaces and retires the outgoing
  engine, resets registers and invalidates batches/wakeups. It keeps every
  queued human input, message and command. A failed commit ends life and
  retires the unused prepared engine.
- `Option<Engine>` makes final retirement consumptive and repeated teardown
  harmless. Each uncertified end records `TeardownFailed` once in the owning
  log; failures propagate through joins to the host. A failed launch returns
  both its cause and teardown failures. Panic is itself a teardown failure.
- Wire `Detach → Ended` certifies engine/worker teardown, not socket EOF.
  Synod recovers its machine factory only after every agent thread joins.

## Uniform spawning and the one snapshot law

**Every agent with fuel may spawn; each fork gives the child one less unit,
without debiting the parent.** `SPAWN_FUEL = 3` bounds depth, not fan-out.
The desk refuses a zero-fuel spawn; its prompt omits the dead spawn family.

`` exarch-agents `start `` takes the closed record
`[prompt, name, type, grant, model]`. Missing or misspelled fields are static
errors; variant decoders enumerate legal runtime tags. It is always
asynchronous: construct, adopt and launch the child, then return a summary;
`` exarch-agents `list `` reads the roster.

- `Shell::bequeath` snapshots the **serialisable fragment** of lexical scope,
  dynamic context and builtin table. Every handle becomes an opaque
  placeholder, including in captured closures, native applied arguments and
  handler arms. Hooks do not cross. No terminal authority crosses.
- The child receives a fresh mailbox, life/exchange scopes, provider handle and
  engine. Its `cd`, environment and new bindings never flow back; reply is
  the sole return path. The fork carries session policy, including inherited
  bindings sealed as baseline and worker-retention terms.
- Identity parks the bequest for `IdentityTransport::adopt_parked`; wire
  seals it and names the bytes in an enquiry. The host asks its `Factory`
  for a fresh engine stream, attaches the seal, waits for `Attached`, then
  adopts the child. Engine/log preparation is outside every inbox lock.
- Both carriers consume the same bequest and scrub, hence the same scope,
  handlers, builtin table and policy. No engine spawns an engine; the host
  relays the sealed bytes without interpreting the parent's session.
- Synod's factory dials the guest daemon's host-CID-only listener, one
  engine per connection. A failed construction drops that stream; no birth
  token table, acknowledgement protocol or expiry is needed
  ([[design/engine-protocol|engine-protocol]], [[map/synod|synod]]).

`type` selects model memory, not shell isolation:

- `` `amnemon `` imports no history; only snapshot and prompt cross.
- `` `mnemon `` imports the parent's turn table and residues through
  `Protocol::Inherited` pointers, retaining lineage ids and file addresses,
  then appends the new prompt. An unanswered tool-call frame is omitted.
  Views are projections of that structure, not copied rendered messages.

`model` is required data: `` `inherit `` takes the current provider;
`` `role <name> `` selects one the human bound at launch with `--role`
(and provider/via qualifiers). Roles carry providers, never grants or
personae. The model discovers no catalog or account. Tuning is fixed at
launch for roles; a later `/model` changes only its addressed agent.
`provider::Bureau` mints selections over a shared `Pool` and credentials
([[map/exarch/provider|provider]]).

Selective delegation is ordinary data flow: bind transcript slices silently,
evict irrelevant context under a note, then name that binding in an
`amnemon` child's short prompt. The slice crosses once in the shell snapshot,
not again interpolated into prose
([[internals/session-record|session-record]]).

## Reply, follow-up and scheduling

**Reply is a faithful value, not prose scraped from a final turn.**

- `` exarch-agents `reply `` stages the value; the complete tool batch ends
  before harvesting it, and cancellation discards it. Quiescing precedes
  reporting. Reply clears schedules, not helpers; the agent may rest.
- A parent binds `` exarch-agents `read <name> `` under its demanded type.
  New parent input makes the helper pending again; a crossing report keeps
  that obligation while the follow-up remains queued.
- With scheduling authority, a conversing agent's wakeup may open a later
  job; a returning agent waits for armed schedules and clears them on report.
  Epochs invalidate queued wakeups across reset. Firing, coalescing and final
  registry retirement occur in one mailbox transaction, preventing false rest.
- Timers retain a weak reference to mailbox shared state, never a temporary
  address wrapper or an owner. Focus and diagnostic reads mutate no lifecycle.

## Permissions: a spawn pushes one layer

**A spawn narrows the parent's stack; it never establishes a new ceiling.**
Its mandatory `grant` is one of
`` `inherit | `confined | `read-only | `edit-only | `reasonable | `restrict R ``.

- Named profiles and explicit `R` are the same act: at most one layer atop
  the parent's whole stack. `` `inherit `` is the no-op top layer; there is
  no spawn `` `dangerous `` spelling.
- The bequest carries `SpawnGrant`. Named bases resolve through
  `policy::base_layer`; explicit records remain undecoded until the child's
  engine freezes them against its own cwd. `SpawnGrant::narrow_onto` is the
  common layering point under either carrier.
- Per-check intersection means a looser child layer cannot grant what the
  parent lacks. A restriction value creates no self-denial file; CLI
  restriction files remain a different authority boundary.
- The snapshot law and the grant law are independent: memory mode changes
  neither shell isolation nor authority
  ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]],
  [[design/grant|grant]], [[design/capability-freeze|capability-freeze]]).

## See also

[[map/exarch/agent|agent]], [[map/exarch/frontend|frontend]],
[[map/synod|synod]], [[internals/cancellation|cancellation]],
[[internals/session-record|session-record]],
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]].
