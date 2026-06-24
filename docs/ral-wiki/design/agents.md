# Agents: roles, spawning, and the narrowed permissions base

**An exarch agent is one continuous [[map/exarch/session|`Session`]] driven by
the single shared `drive` loop; the only differences between agents are
structural flags, and they decompose into two orthogonal axes — whether an agent
may *spawn* children and whether it *returns* a value.** A sub-agent is not a
different machine, only a `Session` forked with different flags and a narrowed
capability ceiling.

## Three roles on two axes

The axes are independent booleans, gating both tool advertisement and dispatch
through one `ToolSet::allows` check ([[map/exarch/tools|tools]]):

- **`spawns()`** — holds the `agent` family (`agent` / `agents` /
  `agent_cancel`). A peer withholds it, so the spawn tree stays **one level
  deep**, advertised and enforced.
- **`replies()`** — holds `reply`. Held by every *returning* agent, withheld
  only from the interactive root, which converses across turns and never returns
  a value ([[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]).

The two booleans pick out the three live roles:

- the **interactive root** — spawns, does not return (`ToolSet::interactive_root`).
  Its human is an ever-present writer, so it parks on an idle inbox rather than
  terminating;
- the **headless (returning) root** — does both (`ToolSet::returning_root`).
  Seeded once, it produces one result and the process exits;
- a **peer** (a spawned sub-agent) — returns, does not spawn (`ToolSet::peer`).

"Returns a value" and "does not park for a human" are the *same fact*, read in
one place: a peer and a headless root both terminate at quiescence, while the
interactive root parks ([[map/exarch/session|session]]).

## Spawning: fork, snapshot, detach

The `agent` tool is **launch-only and always asynchronous**
([[decisions/260617_async-agent-tool|async-agent-tool]]). One call:

- **`fork`s a child `Session`** through `Shell::fork_session`
  ([[map/core/shell-state|the flow matrix]]). The child snapshots the parent's
  whole lexical scope, dynamic context (cwd, env, grants, handlers), and the
  installed builtin table, and starts fresh in everything else — its own inbox,
  a fresh cancel token, no terminal authority. This is a **value snapshot**: the
  child's `cd`, env, and new bindings die with it; there is no flow-back, and the
  parent receives a string, not the child's bindings. The isolation mirrors a
  [[design/pipelines|byte-pipeline stage]]'s subshell;
- **runs it on a detached thread** through the same `drive` loop, returning a
  start receipt `{id, title, status, log_dir}` at once. The child runs off the
  parent's critical path — the one shape in-turn concurrency cannot express, the
  parent turn ending before the child does;
- **delivers the child's single reply later** as a marked `Turn` through the
  parent's [[map/exarch/frontend|inbox]], rendered to prose at the peer edge.

A peer cannot itself spawn, so recursion is bounded to depth 1 by the withheld
spawn family.

## Returning: the deliberate `reply`

A returning agent hands back the argument of an explicit, hard-terminating
**`reply`** tool call — never a scrape of whatever prose ended the run
([[decisions/260622_agent-reply-tool|agent-reply-tool]]). `reply` is the *sole*
return path: a returning agent that finishes without it is re-nudged within
budget, then **fails honestly** rather than handing up a trailing fragment that
masquerades as the answer. The payload is the faithful `serde_json::Value` the
model passed, rendered at each consuming edge by the shared value→text rule
([[map/exarch/shell-eval|shell-eval]]) — prose for a model parent, the structure
itself for the headless harness
([[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]).

## Self-scheduling is inherited

A peer may arm its own wakeups (a cron expression or `after <dur>`) into its own
inbox when the root was launched `--allow-schedule`: `schedule_authority` is
**inherited by a fork**, so the grant flows down the spawn tree. Scheduling is
gated by that authority, not by the `ToolSet` axes
([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]).

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
the authority decision, because [[map/exarch/session|`Session::fork`]] takes the
child's `Capabilities` as an argument rather than cloning the parent's.

## See also

[[design/exarch-architecture|exarch-architecture]] (the agent as a provider loop
over one shell tool), [[design/grant|grant]] (the capability lattice the meet
runs in), [[map/exarch/tools|tools]], [[map/exarch/session|session]],
[[map/exarch/policy|policy]],
[[decisions/260617_async-agent-tool|async-agent-tool]],
[[decisions/260622_agent-reply-tool|agent-reply-tool]],
[[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]].
