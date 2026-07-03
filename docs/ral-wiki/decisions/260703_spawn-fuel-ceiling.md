---
status: active
---

# Spawn fuel: a generous, uniform ceiling on delegation depth

**Unbounded spawn depth is a resource-exhaustion hazard with no structural
floor: a model stuck delegating "redo this" to a fresh child has no reason to
ever stop, and each hop pays a full session boot, a detached OS thread, and a
place in the fleet registry.** This decision gives every agent a `fuel: u32`
that a fork spends one unit of on its child; once an agent's fuel reaches zero,
[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]]'s `Gate` mechanism
removes the spawn tools from its view. This refines, rather than reverses,
that decision's central claim — no agent is privileged by special-case code,
only by tree position — because fuel *is* a pure function of position
(`SPAWN_FUEL - depth`), computed identically for every node.

## Context

[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]] deliberately
deleted a depth-1 spawn cap: "every agent may spawn, so the spawn tree is
unbounded in depth rather than capped at one level." Nothing since has bounded
the other axis — a chain of `agraphos`/`anamnesis` calls, each one a genuine
sub-agent forking a genuine sub-agent, can recurse for as long as a model keeps
choosing to delegate. The only guidance against this is prose in the tool
description ("NEVER delegate a single grep/view/read/edit you can run
inline"), which shapes judgment but enforces nothing. The only numeric
ceilings nearby — `MAX_STEPS` (round-trips within one turn) and
`AGENT_CEILING` (an abandoned worker's one-hour wall-clock reaper) — bound
different axes and do not compose into a depth limit.

## Decision

- **`Agent` carries `fuel: u32`.** The trunk is born with `SPAWN_FUEL = 8`
  (`agent.rs`); `Agent::fork` computes the child's fuel as
  `self.fuel.saturating_sub(1)` and never lets it wrap below zero.
- **A new `Gate::Spawns` axis** (`tools.rs`) admits `agraphos`/`anamnesis` only
  while `fuel > 0`, alongside the existing `Returns` and `Schedules` axes.
  `tools_for` gains a third parameter, `can_spawn`, threaded from each fork's
  freshly computed fuel.
- **Depletion is silent tool absence**, not a refusal. This is the same shape
  `reply` and the self-wakeup family already use: "a tool absent from an
  agent's slice is neither advertised nor dispatchable" (`tools.rs`), so a
  depleted agent's view simply lacks the tool — dispatch needs no new branch,
  and a well-behaved model never sees a call it must recover from.
- **`agents`/`message`/`agent_cancel` stay `Gate::Always`.** They observe and
  manage already-live agents rather than create new ones, so a fuel-exhausted
  agent keeps the ability to coordinate with whatever peers it was told about,
  even though it cannot mint new ones.
- **`/discuss` bypasses tool gating** — it calls the fork primitive directly
  from a host slash command, not through the model-facing dispatch path — so
  `tui_loop.rs` separately refuses to seat a chair when `session.fuel() < 2`:
  the chair needs one unit to be born and a second to spawn its debate
  partner, and seating one with less would strand it with no `agraphos` in its
  own view.
- **Tool descriptions say depth is finite.** `agraphos`/`anamnesis` now state
  that each spawn spends one unit of the caller's own budget on the child, so
  a chain cannot recurse forever.

## Why this shape

- **Bounds the reported hazard, not a different one.** A runaway spawn chain
  (A delegates to B delegates to C, indefinitely) is a depth problem; a single
  agent fanning out many independent children in one turn is a breadth
  problem, already gated by the tool description's cost warning and by each
  spawn being a distinct, described call rather than a loop. This decision
  targets depth only.
- **Uniform, not privileged.** Every agent computes its own gate from its own
  `fuel`, and `fuel` is nothing but `SPAWN_FUEL` minus this agent's distance
  from the trunk — a structural fact identical in kind to `parent`, not an
  `is_root`-style branch reintroduced through the back door.
- **Matches the existing idiom.** `MAX_STEPS` is already "a generous ceiling
  no genuine turn reaches, guarding a different runaway axis." `SPAWN_FUEL`
  is the same shape for generations across the tree instead of round-trips
  within one turn.
- **No new permission predicate.** Gating through `tools_for` reuses the exact
  mechanism `Returns`/`Schedules` already prove out, rather than adding a
  second, dispatch-time check that the comment on `Tool::gate` explicitly
  says the design avoids.

## Alternatives considered

- **A fleet-wide live-agent cap**, bounding total concurrent agents regardless
  of tree shape. Rejected for now: it catches breadth explosions a chain-only
  guard misses, but couples unrelated branches' budgets — one branch's
  legitimate fan-out could starve a sibling's spawn — and breadth is not the
  failure mode this decision responds to. An orthogonal cap remains available
  later if fan-out itself proves hazardous.
- **A runtime refusal inside `dispatch_spawn`** (an `invalid_input`-style
  error returned to a model that calls `agraphos` anyway). Rejected: it
  reintroduces exactly the "separate permission predicate to keep in sync"
  that tool-membership gating exists to avoid, and it teaches a well-behaved
  model a call it must fail and recover from instead of one it never sees.
- **A configurable `--max-agent-depth` flag.** Deferred: no user-facing need
  has surfaced, and a bare constant matches `MAX_STEPS`'s own unconfigured
  shape. A flag can be layered on later without touching the mechanism.

## See also

[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]] (the unbounded-depth
claim this refines), [[decisions/260617_async-agent-tool|async-agent-tool]] (the
earlier, fully-superseded depth-1 cap), [[design/agents|agents]],
[[map/exarch/agent|agent]], [[map/exarch/tools|tools]].
