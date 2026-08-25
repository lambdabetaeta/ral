# Agents: the uniform node, the tree, and the parent-less trunk

**An exarch run is a *fleet* of structurally identical *agents* arranged in a
tree, driven by the single shared `attend` loop; the only thing that distinguishes
one agent from another is its *position* — whether it has a parent.** A sub-agent
is not a different machine: it is an `Agent` ([[map/exarch/agent|agent]]) forked
from a value-snapshot of its parent's shell, with a narrowed capability ceiling
and a `parent: Option<AgentId>` link. The thin `Fleet` holds what every node
shares — the registry, the one event bus, and the transport engine.

## One predicate, fixed at construction

There is no `is_root`. Whether an agent returns a value or converses with a human
is a **construction-fixed `returns` bit** on the `Agent` — `true` for a `fork`ed
sub-agent, `false` for a `/branch` child, `!interactive` at the trunk. One bit is
the single source of truth for every role reader: `returns()`, parking's
conversing predicate, the desk's `reply` refusal
([[map/exarch/builtins|builtins]]), and the per-agent builtin index resolved from
the same bit at `Agent::assemble` — so reply availability, parking, and the
advertised vocabulary cannot disagree
([[decisions/260705_branch-minimal|branch-minimal]]). **Fuel is a separate
construction-fixed depth budget:** an agent's spawn surface is present exactly
when `fuel > 0`; it does not decide whether the agent returns. Position still
does two jobs — it fixes the registry edge (`parent`) and the signal path (only
the trunk's session is minted facing the ambient causes,
[[decisions/260726_cancel-is-a-join|cancel-is-a-join]];
[[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]) — but it does
not decide who returns.

- **A returning agent holds `reply`.** A returning node at any depth, *and* a headless trunk
  (`parent = None`, `interactive = false`) seeded once to produce one result, both
  advertise it and terminate at quiescence. This is
  [[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]'s
  reply gate, read off the construction-fixed bit rather than off
  `is_root && interactive`.
- **A conversing agent had `reply` withheld at construction** and parks for a human
  instead of returning. Its `reply` call is refused at the desk, and the verb is
  dropped from its builtin index, both keyed on the same bit. The interactive
  trunk is one such agent — parent-less, its writer ever-present — but not the
  *only* one: a **branch** is interactive, `reply`-withheld, and — like the
  trunk — parent-less, a root of its own tree rather than a descendant
  ([[decisions/260705_branch-minimal|branch-minimal]]). "Parent-less" and
  "converses" are not the same set, which is exactly why the property is a bit
  fixed at construction rather than a predicate on position.

"Returns a value" and "does not park for a human" remain the *same fact*, read in
one place ([[map/exarch/agent|agent]]). Parking is **computed, not stored** — a
`ParkMode` (`Held` / `Engaged` / `HeldByChildren` / `UntilCancelled` / `Quiesce`)
derived on every wake: a conversing agent parks `Held` while its registry entry
lives, a returning agent a human has exchanged a message with parks `Engaged`
bounded by the registry's idle lease, and everyone else quiesces.

## Prompt obligations follow construction

The prompt is an *agent-invariant base* plus one per-agent resolution step. The
same construction facts that govern the desk govern what the model is taught:

- **`returns` gates `# Agent`.** Returning agents receive the deliberate-return
  contract; conversing agents do not.
- **`interactive` gates `Surfacing`.** Human-facing guidance remains in the
  shared base, so a returning interactive child can receive both it and
  `# Agent`.
- **`fuel > 0` gates `# Agents`.** A leaf receives neither spawn
  guidance nor the spawn family in its builtin index.
- **The builtin index has three bits** — `returns`, `allow_schedule`, and
  `spawns` (`fuel > 0`) — cached once from the booted shell. The unresolved base
  stays on the node, and roots, identity forks, and wire children resolve their
  own prompt before the log bookend records its byte length.

## Uniform spawning: bounded by spawn fuel

Spawning is **available to every agent with fuel left**, so the spawn tree is
not capped at one level. The effective `spawns` bit is `fuel > 0`; when it is
false, the prompt index omits `agents` as a dead family, while the desk remains
the runtime authority. Depth-N works
structurally — a child registers in the fleet's registry and `fork` snapshots
the parent's shell by value at any depth — but each `fork` hands the child one
less unit of `fuel` than the parent holds (the parent's own `fuel` is untouched,
so fan-out itself is unbounded), and a `fuel == 0` agent's spawn call is refused
at the desk with the exhaustion text: a delegation chain terminates by refusal
a fixed number of generations down rather than recursing forever
(uniform-agent-nodes, superseding the
depth-1 cap of [[decisions/260617_async-agent-tool|async-agent-tool]];
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]], bounding the depth
that decision left open).

The `` agents `start `` tag is **launch-only and always asynchronous**
([[decisions/260617_async-agent-tool|async-agent-tool]]). Its argument is a
single closed record `[prompt: …, name: …, type: …, grant: …, search: …]` —
a record literal, so a missing or misspelled field is a *static* error naming
it, while the `type` (`` `amnemon ``/`` `mnemon ``) and `grant` tags are
checked at the runtime door that enumerates their legal labels
([[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]).
One call:

- **`fork`s a child `Agent`** through `Shell::fork_session`
  ([[map/core/shell-state|the flow matrix]]). The child snapshots the parent's
  **serialisable fragment** of its lexical scope, dynamic context (cwd, env,
  grants, handlers), and the installed builtin table, sets `parent` to the
  spawning agent's id, takes the spawn's `name` as its identity, and starts
  fresh in everything else — its own inbox, a fresh cancel token, an owned
  provider handle seeded from the parent's current model, no terminal
  authority. This is a **value snapshot**: the child's `cd`, env, and new
  bindings die with it; there is no flow-back, and the parent receives a
  string, not the child's bindings. The isolation mirrors a
  [[design/pipelines|byte-pipeline stage]]'s subshell — see
  [[#Wire-seat children: the same snapshot, a different wire|below]] for why
  "serialisable fragment" is the exact promise rather than "whole scope";
- **runs it on a detached thread** through the same `attend` loop, answering
  at once with the roster afterwards — the child's row carrying `name`,
  `elapsed-s`, and `log-dir` — a ral record the script can bind and fan out
  over. The child runs off the parent's critical path — the one shape
  in-exchange concurrency cannot express, the parent exchange ending before
  the child does;
- **delivers the child's single reply later** as a marked `Item` through the
  parent's [[map/exarch/frontend|inbox]] — the parent edge `parent` names —
  rendered to prose at the consuming edge.

The spawn's **`type`** field chooses the child's **model memory**, not its shell
isolation ([[decisions/260702_subagent-memory-modes|subagent-memory-modes]]):

- **`` `amnemon ``** is tabula rasa. The child starts with no conversation history;
  only the shell value-snapshot and the chosen prompt cross the edge.
- **`` `mnemon ``** remembers. The child imports the parent's model-visible context
  and appends the call's `prompt` as a fresh final user prompt, while
  reusing the parent's current provider selection so provider prompt caches can
  hit. If the parent is mid-tool-call, the unanswered assistant tool-call frame is
  not inherited; the child forks the request context, not a dangling protocol.

### Bind and hand: context as a value

Selective delegation is ordinary data flow, not a new memory mode. The parent
surveys and reads closed spans, binds the returned transcript without printing
it, slices or reshapes that `Str` in ral, drops the originals, and hands the
binding to an `` `amnemon `` child:

```ral
let ctx = context-read [4, 7]
let handoff = slice ctx 0 12000
context-drop [4, 7]
agents `start [
  prompt: "read `handoff` for the material to work from; report your findings",
  name: 'researcher',
  type: `amnemon,
  grant: `read-only,
  search: false,
]
```

`handoff` crosses in the child's forked scope, not in `prompt`'s text: `prompt`
stays a short instruction naming the binding, so the 12000 characters travel
once, as data, rather than twice — once as a binding and once spliced into the
first model-visible message. The child receives `handoff` through the ordinary
serialisable value snapshot;
the same idiom can recur down the delegation tree. The token cost is paid at
the leaf for the slice that is actually handed over. Small, certainly needed
material may still be spliced into a prompt; a large binding should remain a
binding, since binding is silent while stdout and a final shell value become
model material.

## Wire-seat children: the same snapshot, a different wire

A `Seat::Wire` trunk ([[map/exarch/agent|agent]]) runs its shell in a guest
engine, host-side of a vsock connection
([[decisions/260722_session-is-a-process|session-is-a-process]]) — the desk
that answers `` agents `start `` sits in the host process, and cannot reach a
`Shell` living in another machine the way it reaches into its own nursery.
**The whole of that asymmetry is one field of one enquiry.** Beside the model's
record, `` `start `` carries a `fork` tag saying how the child's forked session
is to be reached: `` `parked <id> ``, a nursery slot the handler adopts by id,
or `` `listening [port, token] ``, a port the guest has bound. Both arms run
every authority check the desk runs before anything is spawned, and both answer
the same roster, so the enquiring builtin cannot tell which one served it.

**The connection is opened by the side that knows what it is for.** A wire
spawn is one exchange:

- the builtin **binds an ephemeral guest port for the duration of this one
  spawn**, packs the scrubbed fork into an `EngineSeed` before the listener
  thread starts — a `Shell` never crosses to it, and the seed is all it ever
  holds of the parent's session — mints eight token bytes, and names port and
  token in the enquiry;
- the desk, *while answering*, dials that port, writes the token, and waits;
- the listener thread checks the token, spawns `current_exe --engine` with the
  dialled connection on its protocol fd and the seed on an inherited one — the
  same re-exec shape a wire seat's own construction uses, one level in — and
  only **then** writes the acknowledgement byte;
- the desk reads the ack, adopts the stream as a `Seat::Wire` child exactly as
  the trunk itself was seated, and hands it to the same `spawn_async` an
  identity trunk's fork reaches.

**A dialling side never has to ask which connection this is.** Correlation is
the price of accepting: a listener anyone may dial must publish a preamble for
callers to name themselves in, hold a table keyed by what that preamble
carries, run a pump to accept into it, expire the entries nobody claims, and
split the spawn into a phase that reserves a child and a phase that redeems it.
Open the connection from the other side and none of that has anything left to
do — the host dialled a port it was told about, in the enquiry it is still
answering, so it already holds the spawn the connection is for. What is left is
not a cheaper correlation mechanism but no correlation at all
([[decisions/260825_the-host-dials-in|the-host-dials-in]]). The token
changes job with the direction: no longer a key naming a rendezvous, it is a
guard — the guest's proof that the connection it is about to hand a seeded
engine was opened by the host, and not by something inside the guest racing it
for the port ([[map/core/io-process|the spawn jail]] permits
`socket(AF_VSOCK)`; the guest kernel's refusal of a guest-local dial is the
standing defence, and the token stands behind it).

**The child exists before the roster names it.** The listener acks only after
`spawn()` has returned, and the desk registers the child only after reading the
ack. There is no window in which a registered agent has no engine behind it —
which is why the roster every tag answers carries no state column, and why
`` agents `list `` reads as a fact rather than as an intent.

A spawn that fails leaves nothing to reconcile. A desk that refuses never
dials, so the builtin wakes its own listener thread rather than leave it in its
poll; a child that will not spawn is never acked, and the reason raised is
whichever stood nearer the failure — the guest thread's if it has one,
otherwise the desk's. Nothing was reserved, so nothing has to expire. Past the
ack the child is alive, and a refusal from there on simply drops the stream:
the child reads EOF on its protocol fd and the guest's own table reaps it.

Depth beyond one needs nothing extra: a wire child inherits the dialler its
parent was built with, so a helper spawning a helper of its own binds its own
port and is dialled exactly as it was, and `fuel` bounds the recursion as it
does in the in-process lattice.

**The one snapshot law.** A wire seed cannot carry a `Value::Handle` binding —
a handle is live authority over a parent-side resource, with no wire form —
while a fork of a scope, left alone, would carry one unfiltered. Rather than
let `` agents `start `` mean two different things depending on which seat
answered it, the scrub lives at `Shell::fork_scrubbed`, the one fork both arms
take: the identity arm is that fork parked in the nursery, the wire arm is that
same fork packed into an `EngineSeed`, so neither snapshots more than the
**serialisable fragment** of the parent's scope. This is why the fork
description above says "serialisable fragment" rather than "whole lexical
scope": every other line of the fork law already denies a child the parent's
cancel domain, its terminal authority, its inbox, its provider handle, and a
live handle is exactly that class of authority. A round-trip test pins the law:
fork a scope in memory, seed the same scope through `EngineSeed`, and the two
children resolve every name to the same value or the same absence.

Past that one field the seat asymmetry ends and the fleet's uniformity resumes:
`` agents `message ``, `` agents `cancel ``, and the idle-lease reaper all
address a descendant by name through the registry, whatever seat it sits on,
and a wire helper's `message` crosses as an enquiry on its own connection
exactly as an identity peer's does — sender and recipient never learn each
other's transport.

## Returning: the deliberate `reply`

A returning agent hands back the argument of an explicit, hard-terminating
**`reply`** call ([[map/exarch/builtins|builtins]]) — never a scrape of
whatever prose ended the run
([[decisions/260622_agent-reply-tool|agent-reply-tool]]). `reply` is the *sole*
return path: a returning agent that finishes without it is re-nudged within
budget, then **fails honestly** rather than handing up a trailing fragment that
masquerades as the answer. `reply` hard-terminates the agent **regardless of
focus** — the conversation ends out from under a human who had `TAB`bed to it,
and focus falls back to its parent — though the run ends only once the
enclosing `ral` call's batch finishes draining, never mid-batch. The payload is
the faithful first-order ral value the model passed (`FOValue`), rendered at
each consuming edge ([[map/exarch/shell-eval|shell-eval]]) — ral surface syntax
for a model parent, ordinary JSON for the headless harness through the
`user_json` projection
([[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]).
Before the returning node disappears, `reply` cancels and reaps its proper
descendants: a parent may choose to abandon unfinished children, but it cannot
leave live agents registered beneath a settled node. This is not a `/clear`;
unrelated siblings keep their generation and may still settle normally.

## Focus is presentation; the idle lease is lifecycle

The human's `TAB` cursor is a plain `AgentId` the TUI moves — purely
presentational, read by neither the registry nor `park_mode`. `TAB`bing to a
tab lets it receive the human's typed lines and own `Esc`, but looking at a
tab keeps nothing alive. What keeps a non-conversing returning child alive
past quiescence is a renewable **idle lease** the registry arms at birth
(`Registration::lease`, one hour), not the human's attention: the one thing
that renews it is a delivered human message (`AgentRegistry::steer`), so a
child a human is actually steering parks `Engaged` and keeps its lease fresh,
while a lease that is never renewed fires at exactly its birth-seeded hour. A
`/branch` child and the trunk carry no lease at all and so never idle-reap.
Neither `TAB`, nor the model-facing `` agents `message `` tag, nor a
`/resources` probe renews anything — enumeration and attention alone can
never immortalise a child.

A leased child that is parked waiting for input and has sat idle for five
minutes demotes out of the `TAB` cycle and the tab bar into a compact matrix
strip, carrying its idle age — a per-frame projection off the registry's
exchange clock, never stored state, and root is never a candidate. `/focus
<name>` reaches a demoted tab directly and re-promotes it into the cycle; the
gesture is presentation only and never touches the lease. When the lease
itself runs out, the reaper cancels the whole subtree at the bound whether or
not a human happens to be looking at it — mere focus was never immunity, and
there is no `TAB`-driven reap to begin with.

## Descendant messages: marked notes, not shared memory

An agent may send a **marked message** by **name** to a proper descendant
through `` agents `message ``. The registry resolves the name to the
recipient's inbox and posts an `AgentMessage`; the recipient sees it at the
next tool boundary as a marked note naming the sender, not as human input. This is
coordination, not a return edge: it does not share shell state, does not grant
authority, and does not wait for an answer. The durable result path remains
`reply`; the durable cancellation path remains `` agents `cancel ``, addressed
by name the same way.

## Cancellation: a key interrupts one exchange; a terminator cascades the subtree

`Esc` and `Ctrl-C` are a **per-tab exchange interrupt** — they unwind only the focused
tab's current exchange, never a subtree and never an agent
([[decisions/260705_cancel-per-tab|cancel-per-tab]]). The subtree cascade survives,
but only behind the **lifecycle terminators**: the `` agents `cancel `` tag, the
per-agent idle-lease reaper, and `/clear`. They share one registry cascade — the
registry is the spawn *tree* (`AgentRegistry::Entry` carries the `parent` link), so
terminating a mid-tree agent reaps everything below it; `/clear` additionally bumps
the generation, dropping a late result or deferred surface batch from a cleared
generation. A settling `reply` still cancels only the returning node's proper
descendants — a parent may abandon unfinished children, but never leave live agents
registered beneath a settled node. This refines
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]: the per-focus cancel
token now interrupts one exchange in place, while the subtree cascade is the
terminators' alone.

In the TUI, that break is a distinct cancelled rail shape: it wears the `╳`
marker used for errors, while the matrix's failure cell remains reserved for
actual failures.

## Self-scheduling is inherited

An agent may arm its own wakeups (a cron expression or `after <dur>`) into its own
inbox when the trunk was launched `--allow-schedule`: the grant is
**inherited by a fork**, so it flows down the spawn tree. Scheduling is
gated by that authority — refused at the desk without it, and the schedule
family is dropped from an ungranted agent's builtin index
([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]). A live self-schedule
is one of the three reasons an agent parks (`ParkMode::UntilCancelled`).

## Permissions: the child's ceiling is `parent ⊓ base`

**Every spawn states the child's ceiling explicitly**, through a *mandatory*
`grant` field naming one of five capability bake-ins (`confined`, `read-only`,
`edit-only`, `reasonable`, `dangerous`; ordered loosest-to-tightest in
[[map/exarch/policy|policy]]). An unknown tag is refused at the runtime door,
naming all five.

The five are a *subset* of the `--base` CLI vocabulary, and the door's
`PERMISSION_LABELS` is where the two part company. A grantable base must admit
the bundled coreutils (`ral_core::uutils`), which spawn by bare name and so
match no directory prefix: `read-only` and `edit-only` name each tool literally
for exactly this reason. A base whose `exec` block is prefixes alone leaves the
child unable to run `ls`, and — the ceiling being non-escalating — with no way
to ask for it back. A human at the CLI can see that and reach for
`--extend-base`; a child can only spend turns discovering it.

The child is born with

```text
  child = parent ⊓ resolve_base(grant)
```

computed by `policy::narrow`, the **meet-sibling** of the root's
`policy::for_invocation` ([[map/exarch/policy|policy]]). Because meet only ever
removes authority and the result is ≤ both operands, the base can **narrow** the
child below the parent but can **never escalate** it past the parent's reach
([[design/grant|the grant lattice]]):

- naming a base *looser* than the parent simply changes nothing — a network-off
  `confined` parent stays offline even under `reasonable`, since `false ⊓ true =
  false`;
- `dangerous` resolves to the lattice top (`Capabilities::root`), so it means
  *no narrowing — inherit the parent's authority verbatim*.

The base is frozen against the child's working directory as it resolves, so the
meet runs on already-resolved [[design/capability-freeze|capabilities]]. The
ceiling is non-escalating by construction: the spawn site, not the child, owns
the authority decision, because [[map/exarch/agent|`Agent::fork_with`]] takes the
child's `Capabilities` as an argument rather than cloning the parent's.

## See also

[[design/exarch-architecture|exarch-architecture]] (the agent as a provider loop
over one `ral` tool),
[[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]
(the record-spec `` agents `start `` tag, names as fleet-unique identity,
schedule labels, commitments retired),
[[design/grant|grant]] (the capability lattice the meet runs in),
[[map/exarch/tools|tools]], [[map/exarch/agent|agent]],
[[map/exarch/policy|policy]],
[[decisions/260617_async-agent-tool|async-agent-tool]],
[[decisions/260622_agent-reply-tool|agent-reply-tool]],
[[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]],
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]],
[[decisions/260705_cancel-per-tab|cancel-per-tab]] (Esc/Ctrl-C are a per-tab exchange
interrupt, not a subtree cascade),
[[decisions/260705_branch-minimal|branch-minimal]] (the conversing child whose
`returns` bit is fixed false at construction),
[[decisions/260825_the-host-dials-in|the-host-dials-in]] (why the guest
listens and the host dials, and what that direction deletes),
[[decisions/260825_the-wire-carries-the-value|the-wire-carries-the-value]] (one
enquiry class per registry, every tag answering the registry's state),
[[map/synod|synod]] (the dialler's landed home, and the helper surface built
over wire-seat children),
[[decisions/260806_exchange-ends-at-fleet-quiescence|synod's exchange ends at
fleet quiescence]] (the product law a wire-seat fleet's caller must satisfy).
