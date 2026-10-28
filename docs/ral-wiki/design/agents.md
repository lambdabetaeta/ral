# Agents: one owner, one address, one forest

**An exarch run is a *fleet* of uniform agents, one tree per root, driven by
one attend loop.** An `Agent` is the attending thread's owner; an
`AgentHandle` is its shared address, never its lifetime. The fleet holds
launch settings, names, a cancellation root and teardown accounting
([[decisions/261028_one-scope-tree-one-inbox|one-scope-tree-one-inbox]],
[[map/exarch/agent|agent]]).

## Role is fixed; parking is computed

**Whom an agent answers is a construction-fixed `Role`, not an `is_root`
flag.**

- `Converses` answers no caller and withholds `reply`: an attended or
  embedded root.
- `Headless` replies once to the process's caller, then leaves attendance.
- `Child { parent: AgentHandle }` replies to its parent, then parks for
  follow-up work. Its parent edge is strong and fixed; downward views are
  fleet lookups filtered by ancestry, not a mutable children list.

Role governs reply admission, prompt obligations and parking. Fuel is a
separate depth bound; it does not decide whether an agent returns.
`Trunk::{Attended, Embedded, Headless}` fixes how roots are driven.

- An attended conversing root parks `Held` for human input.
- A child holding a current reply parks `Engaged` under its follow-up lease.
- Outstanding child obligations give `HeldByChildren`; armed self-schedules
  give `UntilCancelled`; otherwise the node quiesces.
- Life cancellation ends every park. A severed engine runs nothing further.
  Quiescence never wins over a live queued delivery.

## Scope is the control boundary

**Interrupt < stop < end is depth in ral's own cancellation tree.** No
exarch cancellation token is reset at an exchange boundary.

```text
fleet root
└── life
    └── work
        ├── exchange and its continuations
        └── lives of adopted children
```

- *Exchange* is one item taken up by exarch, including nudge continuations.
  TUI Esc / Ctrl-C cancel only the focused exchange with `Interrupted`;
  helpers survive. SIGINT is forwarded to root exchanges.
- *Work* is the current delegation extent. Stop, `/clear` and `reply`
  abandon it with `Cancelled`, replacing it beneath the same life. Children
  adopted in it end structurally, including their descendants.
- *Life* is the owner's extent. End, `/close`, agent cancel and owner drop
  end it; idle lease lapse does so with `TimedOut`. Dropping an owner ends
  its subtree: an address cannot leave live work without its consumer.
- `/quit` and whole-fleet shutdown cancel the fleet root. A new root under
  an ended fleet is born cancelled. Signals supply `Terminated` or `Aborted`
  without a second cancellation path.
- Provider waits read exchange. Identity ral runs nest their dispatch scope
  beneath it; wire dispatchers poll and strike their own dispatch id.
  Cancelling threads write flags and notify, never call into an engine.
  Factory, attach, probe and protocol-write waits retain their own deadlines.

The agent's ral `Engine` owns loose shell workers independently of delegated
work. Stop and `reply` preserve them and their incarnation-vouched completion
batches. The incarnation is a scope under life, parallel to work; `/clear`
cancels and replaces it even if reboot fails, fencing old batches. A successful
reboot terminates the old transport; owner destruction ends the current engine
([[internals/cancellation|cancellation]]).

## One inbox, one provenance rule

**A delivery is taken up only while the scope vouching for it is live.**
The check occurs once at pop, under the inbox's one mutex. Cancellation is
monotone, so stale provenance stays stale.

- Human text and commands are unfenced. Results carry the parent's work
  captured at adoption; wakeups their arming work; nudges their exchange;
  worker batches the engine incarnation.
- Messages carry the scope under which the sender's life hangs. Words from
  a normally ended sender may still arrive; words from an abandoned subtree
  die with that work.
- A mailbox captures destination and provenance together. A delayed spawn
  must adopt through that original work mailbox, never freshen its authority
  after constructing an engine. A refusal leaves no enrolled child.
- Stop recalls queued human text oldest first; other deliveries live or die
  by provenance. Reply keeps queued posts; `/clear` discards all of them.
- Opening an exchange cancels its predecessor. Its scope is minted at pop,
  so Stop or Esc racing take-up cannot be erased. Declining an already
  claimed worker completion opens nothing and preserves a queued nudge.
- A drawn human prompt is recorded before the exchange is polled: a racing
  gesture cannot lose it or start a provider round under a dead scope.
  Mid-exchange steering drains only while that exchange is live.

**Outstanding children are owned debts, not liveness guesses.** Adoption
mints one `Owed`, and a follow-up to a direct child mints another before
delivery. Paying consumes a debt: a reply moves its receipt into the
parent's queue, and the work is busy exactly while receipts are outstanding.
Dropping a debt pays `Cancelled`, a refused post withdraws its debt silently,
and a dropped owner still owing its parent a report posts failure. Each
follow-up is its own receipt, so an old queued reply cannot make a newer
one disappear; abandoned work starts a fresh pool.

## Embedded jobs and the host's address

**A Synod *job* runs from queued human input through fleet settlement or
Stop, distinct from exarch's item-level exchange.**

- `SettledConversation` owns one root and a conversation-lived bus. Its host
  keeps `handle()`, not an owner reference. `await_job` opens only on human
  input; a worker batch alone does not start a job.
- `run(job, sink)` attends at most once. Replied helpers remain leased
  residents but are no longer outstanding work. Dropping an unattended job
  closes it. Self-scheduling and reset-resume are refused.
- `AgentHandle::stop(JobId)` acts only while that job is running and not
  already stopped, under the same inbox lock as closing. It marks stopped
  and abandons work with recall; stale Stop changes nothing.
- A stopped job takes up nothing new. Text sent after Stop stays queued for
  the next job, which has its own id. Every job pumps output until ended
  helpers' teardown is acknowledged; the closing folder walk and “Stopped.”
  cannot overtake those writes or final output
  ([[decisions/260806_exchange-ends-at-fleet-quiescence|job settlement]]).
- Synod keeps Send available while busy. The queue strip shows waiting
  text; transcript position is recorded uptake, not submission. Serialized
  Stop/recall withdrawals restore text in gesture order. Backend `JobDone`
  alone certifies “Stopped.”; no frontend pending-Stop register is needed.
- `end_fleet` ends every root in the shared launch, drops the owner, then
  waits on detached teardown accounting, retaining no launch or factory.
  Synod recovers the machine only after that barrier.

Settlement excludes the attending owner's own tenant even if its life ends;
helpers and unfinished claims must still tear down.

Live means life is not cancelled; torn down means the `Tenancy` has been
released. Retained addresses are neither consumers nor outstanding
teardown. A census entry starts at name claim, covering construction; the
owner's log drops before its `Tenancy` ends and certifies the engine, then
releases name and entry. A wire engine answers `Detach` with `Ended` only
after its engine thread and session workers have torn down. Missing acknowledgment refuses settlement and the folder scan,
rather than treating socket closure as proof of teardown.

## Roots are sessions

**A root has a fresh engine and its own `record.jsonl`; a root is never a
fork.** `Genesis` is the launch's identity-engine recipe, absent on a wire
host that boots no further roots.

- `/new [name]` hatches a blank root; `/branch [name]` imports the focused
  context with a fresh shell. Imported ancestry is recorded through
  `Inherited`, not a spawn-parent edge.
- `/close` writes `SessionClosed` and ends that life permanently; `/quit`
  ends the fleet while leaving roots resumable.
- `--resume` reopens roots with no `SessionClosed`, each with its context
  replayed and shell fresh. Pins, schedules, workers and bindings are not
  restored. Headless resume requires exactly one live root record.
- `/clear` checks engine rebootability before any mutation, then abandons
  work/queue, cancels and replaces the engine incarnation, clears pins,
  reboots, clears the context and rebirths readings.
  An adopted or wire engine refuses before discarding anything. A failed
  boot after admission leaves the old engine with its work/queue discarded.
- Names are fleet-unique reservations claimed before engine/log preparation.
  The owner's `Tenancy` releases them on teardown. Host commands alone open
  roots; a model has no session-opening builtin.

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
- The child receives a fresh inbox, life/work scopes, provider handle and
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

**A returned value is deposited state, not prose scraped from a final turn.**

- `` exarch-agents `reply `` stages a faithful `FOValue`; after the whole
  tool batch, cancellation/severance are checked before harvesting it.
  Reply abandons delegated work, clears schedules, publishes Ready,
  deposits the value and pays every debt taken up to the parent.
- The child parks. Its parent reads the value through
  `` exarch-agents `read <name> ``, checked against the demanded type. A
  headless root instead returns the same value to its harness. Finishing
  without reply is repaired within budget, then fails honestly.
- Historical reply payload remains readable during new work; only the
  completion timestamp answers whether that work has replied. A readable old
  value never suppresses a new attempt's reply obligation.
- A child's one-hour follow-up lease starts at reply, not birth. The lease is a
  `Weak` whose only strong owner is the inbox: new human or agent input
  breaks it atomically with enqueue, so the old lease cannot race it into
  reaping the child, and a reply refuses one while input is queued. Focus,
  reading replies and probes renew nothing; roots have no reply lease.
- Focus and matrix navigation are presentation only. A cancelled exchange
  is visible as cancelled, not a failed matrix cell.
- With `--allow-schedule`, every fork inherits scheduling admission. Each
  entry holds a weak mailbox of its arming work; abandoned work retires the
  entry instead of rearming. Owner abandonment clears schedules; Stop leaves
  callback retirement to observe the dead scope. Synod refuses schedules.

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

[[design/exarch-architecture|exarch-architecture]],
[[map/exarch/agent|agent]], [[map/exarch/frontend|frontend]],
[[map/exarch/builtins|builtins]], [[map/exarch/policy|policy]],
[[decisions/261028_one-scope-tree-one-inbox|one-scope-tree-one-inbox]],
[[decisions/260826_reply-parks|reply-parks]],
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]].
