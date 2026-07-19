# Agents: the uniform node, the tree, and the parent-less trunk

**An exarch run is a *fleet* of structurally identical *agents* arranged in a
tree, driven by the single shared `drive` loop; the only thing that distinguishes
one agent from another is its *position* — whether it has a parent.** A sub-agent
is not a different machine: it is an `Agent` ([[map/exarch/agent|agent]]) forked
from a value-snapshot of its parent's shell, with a narrowed capability ceiling
and a `parent: Option<AgentId>` link. The thin `Fleet` holds what every node
shares — the registry, the one event bus, the focused-agent handle, and whether a
human is attached ([[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]]).

## One predicate, fixed at construction

There is no `is_root`, no `spawns`/`returns` axis pair. Whether an agent returns a
value or converses with a human is a **construction-fixed `returns` bit** on the
`Agent` — `true` for a `fork`ed sub-agent, `false` for a `/branch` child,
`!interactive` at the trunk. One bit is the single source of truth for every
reader: `returns()`, parking's conversing predicate, the desk's `reply` refusal
([[map/exarch/builtins|builtins]]), and the per-agent builtin index resolved from
the same bit at `Agent::assemble` — so reply availability, parking, and the
advertised vocabulary cannot disagree
([[decisions/260705_branch-minimal|branch-minimal]]). Position still does two jobs
— it fixes the registry edge (`parent`) and the signal path (only the trunk
publishes the process cancel slots,
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
`ParkMode` (`Held` / `Focused` / `HeldByChildren` / `UntilCancelled` / `Quiesce`)
derived on every wake: a conversing agent parks `Held` while its registry entry
lives, a focused agent parks because the human is attached to it, and everyone
else quiesces.

## Uniform spawning: bounded by spawn fuel

Spawning is **universal** — every agent may spawn, so the spawn tree is not
capped at one level. There is no `spawns()` axis: every agent holds the one
`agent` spawn verb ([[map/exarch/builtins|builtins]]). Depth-N works structurally — a child
registers in the fleet's registry and `fork` snapshots the parent's shell by
value at any depth — but each `fork` hands its child one less unit of `fuel`
than the parent holds (the parent's own `fuel` is untouched, so fan-out itself
is unbounded), and a `fuel == 0` agent's spawn call is refused at the desk
with the exhaustion text: a delegation chain terminates by refusal a fixed
number of generations down rather than recursing forever
([[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]], superseding the
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
  whole lexical scope, dynamic context (cwd, env, grants, handlers), and the
  installed builtin table, sets `parent` to the spawning agent's id, takes the
  spawn's `name` as its identity, and starts fresh in everything else — its own
  inbox, a fresh cancel token, an owned provider handle seeded from the parent's
  current model, no terminal authority. This is a **value snapshot**: the
  child's `cd`, env, and new bindings die with it; there is no flow-back, and the
  parent receives a string, not the child's bindings. The isolation mirrors a
  [[design/pipelines|byte-pipeline stage]]'s subshell;
- **runs it on a detached thread** through the same `drive` loop, returning a
  receipt `[name: Str, log-dir: Str]` at once — a ral record the
  script can bind and fan out over. The child runs off the
  parent's critical path — the one shape in-turn concurrency cannot express, the
  parent turn ending before the child does;
- **delivers the child's single reply later** as a marked `Turn` through the
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

## Focus: the dynamic human attachment

The human is *attached* to exactly one agent at a time — the **focused** node —
and attachment is dynamic. The `Fleet` owns `focus`; `TAB` moves it (the TUI),
the focused agent receives the human's typed lines as fresh turns and owns `Esc`,
and a de-focused idle agent reaps at quiescence (it is the daemon by another name
if it lingers). Conversing with a sub-agent is real but ephemeral: once it goes
idle and you `TAB` away it reaps, and once it `reply`s it is gone — its tab is a
readable transcript, not a resumable conversation.

## Peer messages: marked notes, not shared memory

Live agents may send one another a **marked message** by **name** through the
`message` builtin. The registry resolves the name to the recipient's inbox and
posts an `AgentMessage`; the recipient sees it at the next tool boundary as a
marked note naming the sender, not as human input. This is
coordination, not a return edge: it does not share shell state, does not grant
authority, and does not wait for an answer. The durable result path remains
`reply`; the durable cancellation path remains `agent-cancel`, addressed by name
the same way.

## Cancellation: a key interrupts one turn; a terminator cascades the subtree

`Esc` and `Ctrl-C` are a **per-tab turn interrupt** — they unwind only the focused
tab's current turn, never a subtree and never an agent
([[decisions/260705_cancel-per-tab|cancel-per-tab]]). The subtree cascade survives,
but only behind the **lifecycle terminators**: the `agent-cancel` builtin, the
per-agent ceiling reaper, and `/clear`. They share one registry cascade — the
registry is the spawn *tree* (`AgentRegistry::Entry` carries the `parent` link), so
terminating a mid-tree agent reaps everything below it; `/clear` additionally bumps
the generation, dropping a late result or deferred surface batch from a cleared
generation. A settling `reply` still cancels only the returning node's proper
descendants — a parent may abandon unfinished children, but never leave live agents
registered beneath a settled node. This refines
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]: the per-focus turn
token now interrupts one turn in place, while the subtree cascade is the
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
[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]]
(the Fleet/Agent split, the `parent` collapse, dynamic focus),
[[design/grant|grant]] (the capability lattice the meet runs in),
[[map/exarch/tools|tools]], [[map/exarch/agent|agent]],
[[map/exarch/policy|policy]],
[[decisions/260617_async-agent-tool|async-agent-tool]],
[[decisions/260622_agent-reply-tool|agent-reply-tool]],
[[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]],
[[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]],
[[decisions/260705_cancel-per-tab|cancel-per-tab]] (Esc/Ctrl-C are a per-tab turn
interrupt, not a subtree cascade),
[[decisions/260705_branch-minimal|branch-minimal]] (the conversing parented child
whose `returns` bit is fixed false at construction).
