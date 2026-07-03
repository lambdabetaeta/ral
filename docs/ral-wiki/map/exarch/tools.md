---
generated_at_commit: 1631d78
generated_at_date: 2026-07-03
covers_paths: [exarch/src/tools.rs, exarch/src/tools/]
---

# Map: exarch / tools

`tools.rs` is the tool registry. **A `Tool` advertises itself to the
[[map/exarch/provider|provider]] (name, description, JSON schema) and dispatches
one parsed JSON input against a live [[map/exarch/agent|`Agent`]], returning
a `SessionToolResult` synchronously** — every tool returns now, so there is no
join phase. Each tool owns its own input parsing and invalid-input UX; nothing
in `provider.rs` or `agent.rs` knows a tool's shape. Adding a tool is a
sibling module under `tools/` listed in `registry()`.

**Tool membership is the gate.** `tools_for(returns, schedules, can_spawn)`
filters the static registry by a small `Gate`: `Always`, `Returns`,
`Schedules`, or `Spawns`. Spawning is universal — every agent may spawn,
so the tree is not capped at one level
([[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]], superseding the
depth-1 `spawns()` axis) — but each `fork` spends one unit of the parent's
`fuel` on the child, and `Gate::Spawns` withholds `agraphos`/`anamnesis` once
an agent's `fuel` reaches zero, so a delegation chain bottoms out rather than
recursing forever ([[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]]).
`reply` is present only for returning agents
([[decisions/260623_reply-terminates-returning-agents]]); self-wakeup tools are
present only under schedule authority.

The tools that ship:

- `ral` (`tools/ral.rs`) — evaluate ral source against the session shell,
  synchronously, through [[map/exarch/shell-eval|`run_shell`]]. Its input is a
  required `cmd` (the ral source) and a required one-line `description` (shown on
  the [[map/exarch/frontend|rail]]; the full `cmd` opens in the collapsible
  tool-call block). A fixed 30s call timeout bounds inline work; anything longer
  belongs in a `spawn` that outlives the turn.
- the **spawn family** — `agraphos` / `anamnesis` / `agents` / `message` /
  `agent_cancel`
  (`tools/agent.rs`). Spawning is universal, but `agraphos`/`anamnesis` are
  gated by `Gate::Spawns` on the agent's own `fuel` (nonzero for every fresh
  fork down to a fixed depth,
  [[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]]); `agents` /
  `message` / `agent_cancel` stay `Gate::Always` since they manage already-live
  agents rather than mint new ones. `agraphos` and `anamnesis` are launch-only
  and always asynchronous
  ([[decisions/260617_async-agent-tool|async-agent-tool]]): each forks a child
  [[map/exarch/agent|agent]] from a value-snapshot of the parent shell, runs it on
  a detached thread through the same `Agent::drive` loop, and returns a start
  receipt at once; the child's single reply is delivered later as a marked
  `Turn` through the parent's [[map/exarch/frontend|inbox]]. They differ only in
  model memory: `agraphos` is tabula rasa, while `anamnesis` imports the parent's
  model-visible context and appends the tool call's prompt as the child's fresh
  final prompt. Both take a **mandatory `permissions`** parameter — one of the
  five [[map/exarch/policy|base]] names (`confined`, `minimal`, `read-only`,
  `reasonable`, `dangerous`) — so every spawn states the child's ceiling
  explicitly. The child is born with `parent ⊓ resolve_base(permissions)`
  (`policy::narrow`): a lattice *meet*, so the base can only **narrow** the child
  below the parent, never escalate it past the parent's authority — naming a base
  looser than the parent simply changes nothing, and `dangerous` is the lattice
  top, meaning *inherit the parent's authority verbatim*. `agents` lists live
  workers (id, title, elapsed, log dir); `message` posts a marked note to a live
  agent id through its inbox; `agent_cancel` stops one by id and cascades to its
  subtree. A child may itself spawn, each fork spending one unit of the
  parent's `fuel` on it. The whole sub-agent model — the `parent` predicate,
  spawning, marked peer messages, returning, narrowing, and memory mode — is
  [[design/agents|agents]].
- `spawn_discussion` (`tools.rs` → `tools/agent.rs`) — the host-only helper
  behind `/discuss`, not a model-advertised tool. It spawns an `anamnesis`
  returning chair with the focused context and instructs that chair to spawn one
  `agraphos` partner, consume the partner's ordinary `reply`, and return one
  `result` to its parent ([[decisions/260702_discuss-command|discuss-command]]).
  It calls the fork primitive directly, bypassing `Gate::Spawns`, so the
  `/discuss` command in `tui_loop.rs` separately refuses to seat a chair when
  the focused agent's `fuel` is below 2 — the chair needs a unit to be born and
  a second to spawn its own partner
  ([[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]]).
- `reply` (`tools/reply.rs`), gated by `replies()` — a returning agent's
  deliberate return value ([[decisions/260622_agent-reply-tool|agent-reply-tool]],
  extended to the headless trunk by
  [[decisions/260623_reply-terminates-returning-agents]]). Its `result` argument
  is stashed on the agent as the *faithful* value the model passed and lifted
  into a `Replied` terminal once the tool-call batch drains — it hard-terminates
  the agent regardless of focus and cancels any live descendants before the node
  settles. Each consumer renders it at its own edge by the shared value→text rule
  ([[map/exarch/shell-eval|shell-eval]]'s `json_to_text`: a string passes through
  raw, an object/array is pretty-printed), except the headless harness, which
  writes the structure faithfully to its json `result`.
  `reply` is the *sole* return path — there is no prose scrape — so a returning
  agent that never calls it returns nothing and fails (re-nudged within the
  [[map/exarch/agent|nudge]] budget first). Withheld only from the conversing
  (interactive) trunk.
- the **schedule family** — `schedule` / `schedules` / `unschedule`
  (`tools/schedule.rs`) — self-armed wakeups (a cron expression or `after <dur>`)
  posted into the agent's *own* inbox. Gated by schedule authority, so a
  sub-agent may wake itself when the trunk was launched `--allow-schedule`.
- `fff` (`tools/fff.rs`) — frecency-ranked fuzzy filename search over the working
  tree via the `fff-search` crate. The first call per directory blocks on a
  background scan, then caches a `FilePicker` (scan thread + filesystem watcher)
  in a process-global registry keyed by canonical path; later calls — including
  forked children sharing the cwd — reuse the live index. Databases live under a
  per-pid `$TMPDIR` directory.

Editing and content search are *not* tools: the model runs them as ral host
atoms inside `ral` — see [[map/exarch/builtins|builtins]].
