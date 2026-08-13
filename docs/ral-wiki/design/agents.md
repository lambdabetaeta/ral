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

- **A returning agent holds `reply`.** A peer at any depth, *and* a headless trunk
  (`parent = None`, `interactive = false`) seeded once to produce one result, both
  advertise it and terminate at quiescence. This is
  [[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]'s
  reply gate, read off the construction-fixed bit rather than off
  `is_root && interactive`.
- **A conversing agent had `reply` withheld at construction** and parks for a human
  instead of returning. Its `reply` call is refused at the desk, and the verb is
  dropped from its builtin index, both keyed on the same bit. The interactive
  trunk is one such agent — parent-less, its writer ever-present — but not the
  *only* one: a **branch** is interactive, `reply`-withheld, and *parented*
  ([[decisions/260705_branch-minimal|branch-minimal]]). "Parent-less trunk" and
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
- **`fuel > 0` gates `# Spawning agents`.** A leaf receives neither spawn
  guidance nor the spawn family in its builtin index.
- **The builtin index has three bits** — `returns`, `allow_schedule`, and
  `spawns` (`fuel > 0`) — cached once from the booted shell. The unresolved base
  stays on the node, and roots, identity forks, and wire children resolve their
  own prompt before the log bookend records its byte length.

## Uniform spawning: bounded by spawn fuel

Spawning is **available to every agent with fuel left**, so the spawn tree is
not capped at one level. The effective `spawns` bit is `fuel > 0`; when it is
false, the prompt index omits `agent`, `agents`, `message`, and `agent-cancel`
as a dead family, while the desk remains the runtime authority. Depth-N works
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

The `agent` spawn verb is **launch-only and always asynchronous**
([[decisions/260617_async-agent-tool|async-agent-tool]]). Its argument is a
single closed record `[prompt: …, name: …, type: …, grant: …]` — a record
literal, so a missing or misspelled field is a *static* error naming it, while
the `type` (`` `amnemon ``/`` `mnemon ``) and `grant` tags are checked at the
runtime door that enumerates their legal labels
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
- **runs it on a detached thread** through the same `attend` loop, returning a
  receipt `[name: Str, log-dir: Str]` at once — a ral record the
  script can bind and fan out over. The child runs off the
  parent's critical path — the one shape in-exchange concurrency cannot
  express, the parent exchange ending before the child does;
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
agent [prompt: handoff, name: 'researcher', type: `amnemon, grant: `read-only, search: false]
```

The child receives `handoff` through the ordinary serialisable value snapshot;
the same idiom can recur down the delegation tree. The token cost is paid at
the leaf for the slice that is actually handed over. Small, certainly needed
material may still be spliced into a prompt; a large binding should remain a
binding, since binding is silent while stdout and a final shell value become
model material.

## Wire-seat children: the same snapshot, a different wire

A `Seat::Wire` trunk ([[map/exarch/agent|agent]]) runs its shell in a guest
engine, host-side of a vsock connection
([[decisions/260722_session-is-a-process|session-is-a-process]]) — the desk
that answers `agent-start` sits in the host process, and cannot reach a
`Shell` parked in another machine the way it reaches into its own nursery. A
wire trunk's spawn is therefore **two-phase**, both phases plain `FOValue` on
the existing enquiry channel, and every authority check the desk runs today
still runs first, before anything is spawned:

1. `` `agent-start `` — as an identity trunk's, but on a wire trunk the desk
   mints a token, registers a pending hatch reserving the child's name, and
   answers `` `hatch [token, port] `` instead of adopting a nursery shell
   directly.
2. The builtin, seeing `` `hatch ``, runs the **hatch**: the parent engine
   dials the host on the named port, writes a 16-byte preamble binding the
   dial to the token, and spawns `current_exe --engine` with the dial handed
   down as its protocol socket — the same re-exec shape a wire seat's own
   construction already uses, one level in. It hands the child an
   `EngineSeed` — the nursery-parked fork's scope, context, and validated
   grant tag — over an inherited fd, then enquires `` `agent-hatched [token] ``.
   The desk correlates the guest's dial by token through the **hatchery**, a
   capability object the desk holds rather than a dependency on the
   machine layer directly, adopts the stream as a `Seat::Wire` child exactly
   as the trunk itself was seated, and hands it to the same `spawn_async` an
   identity trunk's fork reaches. The enquiring builtin cannot tell which arm
   served it — the same `` `started [name, log-dir] `` receipt comes back
   either way.

A hatch that fails — the spawn errors, the dial is refused, the builtin dies
before either enquiry lands — enquires `` `agent-abort [token] ``, and the
desk drops the pending hatch and frees the reserved name; an abandoned
pending hatch also expires on its own clock. Depth beyond one needs nothing
extra: a wire child's own desk is host-side too, so a helper spawning a
helper of its own rides the same hatchery, and `fuel` bounds the recursion
exactly as the in-process lattice does.

**The one snapshot law.** A wire seed cannot carry a `Value::Handle` binding —
a handle is live authority over a parent-side resource, with no wire form —
while an in-process fork, left alone, would carry one unfiltered. Rather than
let `agent` mean two different things depending on which seat answered it,
`fork_into_nursery` — the one place both an identity fork and a wire hatch
pass through — scrubs every handle-carrying binding before parking the fork,
so an identity fork and a wire seed's `EngineSeed` snapshot the same
**serialisable fragment** of the parent's scope, never the unfiltered whole.
This is why the fork description above says "serialisable fragment" rather
than "whole lexical scope": every other line of the fork law already denies a
child the parent's cancel domain, its terminal authority, its inbox, its
provider handle — a live handle is exactly that class of authority, and this
closes the one place it had still been slipping through. A round-trip test
pins the law: fork a scope in memory, seed the same scope through
`EngineSeed`, and the two children resolve every name to the same value or
the same absence.

The hatchery is where the seat asymmetry ends and the fleet's uniformity
resumes: peer `message`, `agent-cancel`, and the idle-lease reaper all
address a child by name through the registry, whatever seat it sits on, and
a wire helper's `message` crosses as an enquiry on its own connection exactly
as an identity peer's does — sender and recipient never learn each other's
transport.

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
Neither `TAB`, nor the model-facing `message` builtin, nor a `/resources`
probe renews anything — enumeration and attention alone can never immortalise
a child.

A leased child that is parked waiting for input and has sat idle for five
minutes demotes out of the `TAB` cycle and the tab bar into a compact matrix
strip, carrying its idle age — a per-frame projection off the registry's
exchange clock, never stored state, and root is never a candidate. `/focus
<name>` reaches a demoted tab directly and re-promotes it into the cycle; the
gesture is presentation only and never touches the lease. When the lease
itself runs out, the reaper cancels the whole subtree at the bound whether or
not a human happens to be looking at it — mere focus was never immunity, and
there is no `TAB`-driven reap to begin with.

## Peer messages: marked notes, not shared memory

Live agents may send one another a **marked message** by **name** through the
`message` builtin. The registry resolves the name to the recipient's inbox and
posts an `AgentMessage`; the recipient sees it at the next tool boundary as a
marked note naming the sender, not as human input. This is
coordination, not a return edge: it does not share shell state, does not grant
authority, and does not wait for an answer. The durable result path remains
`reply`; the durable cancellation path remains `agent-cancel`, addressed by name
the same way.

## Cancellation: a key interrupts one exchange; a terminator cascades the subtree

`Esc` and `Ctrl-C` are a **per-tab exchange interrupt** — they unwind only the focused
tab's current exchange, never a subtree and never an agent
([[decisions/260705_cancel-per-tab|cancel-per-tab]]). The subtree cascade survives,
but only behind the **lifecycle terminators**: the `agent-cancel` builtin, the
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

## Self-scheduling is inherited

A peer may arm its own wakeups (a cron expression or `after <dur>`) into its own
inbox when the trunk was launched `--allow-schedule`: the grant is
**inherited by a fork**, so it flows down the spawn tree. Scheduling is
gated by that authority — refused at the desk without it, and the schedule
family is dropped from an ungranted agent's builtin index
([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]). A live self-schedule
is one of the three reasons an agent parks (`ParkMode::UntilCancelled`).

## Permissions: the child's ceiling is `parent ⊓ base`

**Every spawn states the child's ceiling explicitly**, through a *mandatory*
`grant` field naming one of the six capability bake-ins — the same
vocabulary as the `--base` CLI flag (`confined`, `minimal`, `read-only`,
`edit-only`, `reasonable`, `dangerous`; ordered loosest-to-tightest in
[[map/exarch/policy|policy]]). An unknown tag is refused at the runtime door,
naming all six. The child is born with

```text
  child = parent ⊓ resolve_base(grant)
```

computed by `policy::narrow`, the **meet-sibling** of the root's
`policy::for_invocation` ([[map/exarch/policy|policy]]). Because meet only ever
removes authority and the result is ≤ both operands, the base can **narrow** the
child below the parent but can **never escalate** it past the parent's reach
([[design/grant|the grant lattice]]):

- naming a base *looser* than the parent simply changes nothing — a network-off
  `confined` parent stays offline even under `minimal`, since `false ⊓ true =
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
over one shell tool),
[[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]
(the one record-spec `agent` verb, names as fleet-unique identity, schedule
labels, commitments retired),
[[design/grant|grant]] (the capability lattice the meet runs in),
[[map/exarch/tools|tools]], [[map/exarch/agent|agent]],
[[map/exarch/policy|policy]],
[[decisions/260617_async-agent-tool|async-agent-tool]],
[[decisions/260622_agent-reply-tool|agent-reply-tool]],
[[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]],
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]],
[[decisions/260705_cancel-per-tab|cancel-per-tab]] (Esc/Ctrl-C are a per-tab exchange
interrupt, not a subtree cascade),
[[decisions/260705_branch-minimal|branch-minimal]] (the conversing parented child
whose `returns` bit is fixed false at construction),
[[map/synod|synod]] (the hatchery's landed home, and the helper surface built
over wire-seat children),
[[decisions/260806_exchange-ends-at-fleet-quiescence|synod's exchange ends at
fleet quiescence]] (the product law a wire-seat fleet's caller must satisfy).
