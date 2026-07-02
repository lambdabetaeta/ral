# Agents: the uniform node, the tree, and the parent-less trunk

**An exarch run is a *fleet* of structurally identical *agents* arranged in a
tree, driven by the single shared `drive` loop; the only thing that distinguishes
one agent from another is its *position* — whether it has a parent.** A sub-agent
is not a different machine: it is an `Agent` ([[map/exarch/agent|agent]]) forked
from a value-snapshot of its parent's shell, with a narrowed capability ceiling
and a `parent: Option<AgentId>` link. The thin `Fleet` holds what every node
shares — the registry, the one event bus, the focused-agent handle, and whether a
human is attached ([[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]]).

## One predicate, read through position

There is no `is_root`, no `spawns`/`returns` axis pair. Every asymmetry reduces
to one structural fact — `parent = None` (the **trunk**) — read together with the
fleet's `interactive` and `focus`:

- **The conversing trunk** is the parent-less node of an interactive fleet
  (`parent = None ∧ fleet.interactive`). It is the *sole distinguished agent*: it
  converses with a human across turns, so it has nowhere to return a value and
  **withholds `reply`**, and it parks unconditionally because its writer is
  ever-present.
- **Everyone else returns.** `returns(a) ⟺ ¬(a.parent = None ∧ fleet.interactive)`
  — a peer at any depth, *and* a headless trunk (`parent = None`,
  `interactive = false`) seeded once to produce one result — advertises `reply`.
  This is exactly
  [[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]'s
  `is_root && interactive`, with `is_root` re-read as `parent = None`.

"Returns a value" and "does not park for a human" are the *same fact*, read in
one place ([[map/exarch/agent|agent]]): a peer and a headless trunk both terminate
at quiescence, while the conversing trunk and the human's currently-focused agent
park. Parking is **computed, not stored** — the deleted `park_when_idle` flag
becomes a `ParkMode` (`Held` / `UntilCancelled` / `Quiesce`) derived from
`parent`/`interactive`/`focus`/`schedules` on every wake.

## Uniform spawning: the tree is unbounded in depth

Spawning is **universal** — every agent may spawn, so the spawn tree is unbounded
in depth rather than capped at one level. The old `spawns()` tool-set axis is
gone; the spawn family is held by every agent, and the tool view differs only in
`reply` and optional self-scheduling ([[map/exarch/tools|tools]]). Depth-N already
worked structurally — a child registers in the fleet's registry and `fork`
snapshots the parent's shell by value at any depth; only the withheld tool capped
it ([[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]], superseding the
depth-1 cap of [[decisions/260617_async-agent-tool|async-agent-tool]]).

The spawn tools are **launch-only and always asynchronous**
([[decisions/260617_async-agent-tool|async-agent-tool]]). One call:

- **`fork`s a child `Agent`** through `Shell::fork_session`
  ([[map/core/shell-state|the flow matrix]]). The child snapshots the parent's
  whole lexical scope, dynamic context (cwd, env, grants, handlers), and the
  installed builtin table, sets `parent` to the spawning agent's id, and starts
  fresh in everything else — its own inbox, a fresh cancel token, an owned
  provider handle seeded from the parent's current model, no terminal authority.
  This is a **value snapshot**: the child's `cd`, env, and new bindings die with
  it; there is no flow-back, and the parent receives a string, not the child's
  bindings. The isolation mirrors a [[design/pipelines|byte-pipeline stage]]'s
  subshell;
- **runs it on a detached thread** through the same `drive` loop, returning a
  start receipt `{id, title, status, log_dir}` at once. The child runs off the
  parent's critical path — the one shape in-turn concurrency cannot express, the
  parent turn ending before the child does;
- **delivers the child's single reply later** as a marked `Turn` through the
  parent's [[map/exarch/frontend|inbox]] — the parent edge `parent` names —
  rendered to prose at the consuming edge.

Two spawn tools choose the child's **model memory**, not its shell isolation:

- **`agraphos`** is tabula rasa. The child starts with no conversation history;
  only the shell value-snapshot and the chosen prompt cross the edge.
- **`anamnesis`** remembers. The child imports the parent's model-visible context
  and appends the tool call's `prompt` as a fresh final user prompt, while
  reusing the parent's current provider selection so provider prompt caches can
  hit. If the parent is mid-tool-call, the unanswered assistant tool-call frame is
  not inherited; the child forks the request context, not a dangling protocol.

## Returning: the deliberate `reply`

A returning agent hands back the argument of an explicit, hard-terminating
**`reply`** tool call — never a scrape of whatever prose ended the run
([[decisions/260622_agent-reply-tool|agent-reply-tool]]). `reply` is the *sole*
return path: a returning agent that finishes without it is re-nudged within
budget, then **fails honestly** rather than handing up a trailing fragment that
masquerades as the answer. `reply` hard-terminates the agent **regardless of
focus** — the conversation ends out from under a human who had `TAB`bed to it,
and focus falls back to its parent. The payload is the faithful
`serde_json::Value` the model passed, rendered at each consuming edge by the
shared value→text rule ([[map/exarch/shell-eval|shell-eval]]) — prose for a model
parent, the structure itself for the headless harness
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

Live agents may send one another a **marked message** by `AgentId` through the
`message` tool. The registry resolves the recipient's inbox and posts an
`AgentMessage`; the recipient sees it at the next tool boundary as
`[EXARCH AGENT id MESSAGE: title] body [/EXARCH]`, not as human input. This is
coordination, not a return edge: it does not share shell state, does not grant
authority, and does not wait for an answer. The durable result path remains
`reply`; the durable cancellation path remains `agent_cancel`.

## Cancellation cascades the subtree

`Esc` and `agent_cancel` cancel the focused agent **and its whole subtree** through
one registry cascade — the same cascade the per-agent ceiling reaper uses. The
registry is the spawn *tree* (`AgentRegistry::Entry` carries the `parent` link),
so cancelling a mid-tree agent reaps everything below it. A settling `reply`
cancels only the returning node's proper descendants, while `/clear` cascades
cancel to the focused agent's subtree and bumps the generation, dropping a late
result or deferred surface batch from a cleared generation. This generalises
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] from one root-turn
token to a per-focus token over a subtree.

## Self-scheduling is inherited

A peer may arm its own wakeups (a cron expression or `after <dur>`) into its own
inbox when the trunk was launched `--allow-schedule`: `schedule_authority` is
**inherited by a fork**, so the grant flows down the spawn tree. Scheduling is
gated by that authority, not by tool-set membership
([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]). A live self-schedule
is one of the three reasons an agent parks (`ParkMode::UntilCancelled`).

## Permissions: the child's ceiling is `parent ⊓ base`

**Every spawn states the child's ceiling explicitly**, through a *mandatory*
`permissions` parameter naming one of the five capability bake-ins — the same
vocabulary as the `--base` CLI flag (`confined`, `minimal`, `read-only`,
`reasonable`, `dangerous`; ordered loosest-to-tightest in
[[map/exarch/policy|policy]]). The child is born with

```text
  child = parent ⊓ resolve_base(permissions)
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
the authority decision, because [[map/exarch/agent|`Agent::fork`]] takes the
child's `Capabilities` as an argument rather than cloning the parent's.

## See also

[[design/exarch-architecture|exarch-architecture]] (the agent as a provider loop
over one shell tool), [[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]]
(the Fleet/Agent split, the `parent` collapse, dynamic focus),
[[design/grant|grant]] (the capability lattice the meet runs in),
[[map/exarch/tools|tools]], [[map/exarch/agent|agent]],
[[map/exarch/policy|policy]],
[[decisions/260617_async-agent-tool|async-agent-tool]],
[[decisions/260622_agent-reply-tool|agent-reply-tool]],
[[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]].
