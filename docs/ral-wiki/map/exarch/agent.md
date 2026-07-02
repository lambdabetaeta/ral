---
generated_at_commit: 012fcb2
generated_at_date: 2026-07-02
covers_paths: [exarch/src/agent.rs, exarch/src/event.rs, exarch/src/fleet.rs, exarch/src/nudge.rs, exarch/src/digest.rs]
---

# Map: exarch / agent

`agent.rs` (was `session.rs`) is the turn driver. An `Agent` is the **uniform
node** of a run: the canonical [[map/exarch/frontend|event log]] (`AgentLog`),
the persistent [[map/core/shell-state|`Shell`]], the agent `Capabilities`, its
own inbox, tools, nudger, `cancel::Token`, and an owned hot-swappable
`ProviderHandle`. What every node *shares* — the registry, the one
[[map/exarch/frontend|`FleetBus`]], the focused-agent handle, and whether a human
is attached — lives on the thin [[#The Fleet|`Fleet`]], not on the node
([[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]]). There is no
`Session`, no `is_root`: every distinction reduces to **position in the tree**,
read from `parent: Option<AgentId>` together with the fleet's
`interactive`/`focus`. Output caps are fixed `digest.rs` constants, not per-agent
state.

The **trunk** is the parent-less node (`parent = None`). When the fleet is
`interactive` the trunk *converses* — it withholds `reply` and parks
unconditionally for its ever-present human — but both behaviours fall out of
position, never an `is_root` branch:

```
  returns(a)    ⟺  ¬(a.parent = None ∧ fleet.interactive)   // everyone but the conversing trunk
  park_mode(a)  =  Held           if conversing(a) ∨ fleet.focus = a.id
                   UntilCancelled if a.schedules.armed()
                   Quiesce        otherwise
```

`returns` (`agent.rs:998`) is the inverse of `parent = None ∧ interactive` — the
old `is_root && interactive` reply gate
([[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]),
re-read through tree position. `park_mode` (`agent.rs:1007`, returning a
`ParkMode` of `Held` / `UntilCancelled` / `Quiesce`, `bus.rs:45`) replaces the
deleted `park_when_idle` flag: a present human (the conversing trunk, or the
agent the human `TAB`bed to) holds the node parked; a live self-schedule holds it
until cancelled; otherwise it terminates at quiescence — the one-shot contract a
headless trunk and a settled sub-agent both satisfy.

## The drive loop

Three nested loops, the same for trunk and child alike:

- `drive` — the per-agent lifetime. The trunk publishes its sticky cancel token
  for the OS-signal path (`cancel::publish`, held for the whole drive, replacing
  the old per-turn `mint_root`); a sub-agent publishes nothing, since its token
  is reached through the registry cascade, not the slot. Each pass pulls the next
  turn from this agent's [[map/exarch/frontend|inbox]] via
  `next_or_idle(|| self.park_mode(), …)`, which **re-evaluates the park verdict
  on every `Condvar` wake** — so a `TAB` that de-focuses this agent (and notifies
  its inbox) flips it from `Held` to `Quiesce` and it reaps. A genuine
  turn boundary resets the nudge latches and clears the sticky cancel token
  (`cancel::Token::reset`), so a prior turn's Esc cannot bleed into the next; a
  self-nudge is the same turn continuing and resets neither. `reply` hard-
  terminates the loop regardless of focus or a self-armed schedule — the agent
  returns its value and is gone. At the single exit the loop winds a stranded
  prompt back through `quiesce` so the next `append_user` is always admissible
  ([[invariants/turn-ends-ready|turn-ends-ready]]); the trunk then deregisters
  itself (a child is removed by its spawn site through `settle`), so the fleet
  empties when the last agent leaves. On a caught worker panic (`pump` →
  `Ok(None)`) it rebuilds the live shell's dynamic context from the
  `durable: Mobile` snapshot the worker refreshed at the last clean tool-call
  boundary, rolling the panicking call's grant/env/cwd/handler effects back while
  completed calls' bindings survive
  ([[decisions/260612_exarch-panic-recovery|panic-recovery]]; the IO half is
  core's `TurnGuard`, restored when the turn unwinds —
  [[internals/a-turn-end-to-end|a turn, end to end]]).
- `apply` — one provider round-trip loop over the agent's *own* provider
  (`self.provider.current()`, read once at the top of the turn so a `/model` swap
  lands next turn, never mid-turn): render the transcript, stream a reply through
  `provider.complete`, **admit** then append the assistant message, dispatch any
  tool calls, append their results, optionally append a drained steering prompt,
  repeat until the model emits no tool call. The admission step (`admit_assistant`,
  at the commit boundary) enforces the [[invariants/transcript-admission|transcript-admission invariant]]:
  it repairs a non-object tool-call `fn_arguments` to `{}` (X2) and substitutes a
  stub for an otherwise-empty assistant message (X7), so every committed message
  serialises to a request a strict backend accepts. `StopReason::MaxTokens`
  *with no captured tool call* raises `ProviderError::Truncated` after appending
  the partial, so a `continue` nudge keeps the work as context; *with* captured
  tool calls it dispatches them and continues instead, since returning `Truncated`
  there would strand the protocol in `AwaitingToolResults` and fail the nudge's
  next `append_user` (X6). A hard `MAX_STEPS` ceiling (250) ends a turn whose
  model never stops calling tools — the headless/autonomous counterpart to
  interactive Esc — returning `TurnOutcome::Capped`. That outcome matches no
  nudge rule, so the driver treats it as terminal; re-driving would only spend
  the ceiling again.
- `dispatch` — runs the turn's tool-call batch in order. Every tool returns a
  `SessionToolResult` synchronously; the spawn tools return a start receipt after
  launching their detached child. Once every requested tool id has a result,
  dispatch drains this agent's [[map/exarch/frontend|inbox]]'s tool-boundary
  steering. A non-slash steering prompt is appended after the complete
  tool-result batch, and the next loop asks the provider with the user's steering
  in context
  ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]]). A
  sub-agent has no human writer, so its inbox holds no steering and this is always
  empty.

The headless-completion gate is gone with `expect_action`
([[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]]): the one role flag
that did not fit the `parent` collapse is dropped, not relocated. The nudges that
remain — `must_reply` for a returning agent (`returns()`), `continue` on
truncation, the periodic pin reminder — are driven off the same `react` rule.

## The Fleet

`fleet.rs` is the thin run-as-a-whole: `{ agents: AgentRegistry, bus: FleetBus,
focus: Arc<AtomicU64>, interactive: bool }`. It owns no turn logic — the trunk
and each child drive themselves; the fleet is only where the frontend reads "all
live agents", "which one the human is attached to", "which inbox receives a
marked `message`", and "the bus to drain". The
frontend ([[map/exarch/frontend|`tui::run`]] / `headless::run`) builds it from
handles the trunk already minted at construction, so fleet and nodes never
disagree about what is shared.

- **`alive() ⟺ registry non-empty`** — the literal "dies when no active agents
  remain". An agent removes itself at termination (`reply`, quiescence, cancel);
  the conversing trunk stays until `/quit` because it parks; a headless trunk
  leaves at quiescence; a sub-agent leaves on settle. There is no human-less
  daemon: nothing lingers without a present human, running work, or a bounded
  self-schedule.
- **Focus is dynamic.** `focus: Arc<AtomicU64>` names the attached agent;
  `NO_FOCUS` (`AgentId::MAX`) is the sentinel — off the TUI it never moves, and in
  the TUI it means the frontend resolves focus to the trunk. `TAB` moves it
  (the TUI), notifying the previous and new focused inboxes; the focused agent
  receives the human's typed lines as fresh turns and owns `Esc`. A de-focused
  idle agent wakes, finds `park_mode` now `Quiesce`, and reaps. A returning
  agent's `reply` ends it even mid-conversation; the TUI then **falls focus back
  to its parent**, recursing to the trunk.

## Cancellation cascades the subtree

The single cascade serves three callers — `agent_cancel`, the per-agent ceiling,
and `Esc`. `AgentRegistry::Entry` carries a `parent` link, so the registry is the
spawn *tree*: `AgentRegistry::cancel(id)` walks descendants and cancels the whole
subtree, and `clear_subtree(root)` reaps a subtree and bumps the generation, so a
late result from a cleared generation is still dropped. `Esc` targets
`fleet.focus`'s turn and its subtree (not "the root") — the focused agent's
published token is cleared each turn boundary (`Token::reset`) and the cascade
carries the cancel down. This generalises
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] from one root-turn
token to a per-focus token over a subtree.

## The provider is per-agent and hot-swappable

`ProviderHandle` is owned by the `Agent`, not threaded through `drive`'s
parameters. `drive` reads `self.provider.current()` once per turn. `/model` swaps
the **focused** agent's handle directly on the UI thread (via the registry), so a
swap on one agent never disturbs another. `fork` seeds the child's own handle
from the parent's current provider (`ProviderHandle::new(self.provider.current())`),
so the child inherits the model in force at spawn and may diverge afterward.

## Lifecycle: clear, compact, fork

`clear` rebuilds the focused agent without carrying cancellation residue forward:
it obtains a fresh shell from `boot_root_shell` (the scratch-seeding wrapper
around `bootstrap::boot_shell`, where stale-interrupt discard and cancel
re-chaining live), truncates and restarts the event log, clears the schedule
registry, and cascades cancel to its subtree. It is the focused agent's, not a
fleet-wide reset.

`compact` runs `provider.summarize` over the history when it crosses
`COMPACT_THRESHOLD` (`digest.rs`, 500 KiB) and `AgentLog::can_compact` holds (no
pending tool results). It is called at the **top of `apply`**, where the agent is
`ReadyForUser` ([[invariants/turn-ends-ready|turn-ends-ready]]) and the gate
actually holds — every provider round-trip passes through here, so long
autonomous and headless turns stay bounded without an interactive `/compact`. A
turn-boundary Esc bails before the summarize request
([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).

`fork` builds the child `Agent` for [[design/agents|sub-agent spawning]] through
`Shell::fork_session` ([[map/core/shell-state|the flow matrix]]) rather than
hand-copying fields after a bare `Shell::new`. It takes the child's
`Capabilities` **as an argument**, so the spawn site owns the authority decision
(the parent's verbatim, or `parent ⊓ base` via [[map/exarch/policy|`policy::narrow`]]).
The child sets `parent: Some(self.id)` — the tree edge that routes its result and
drives the subtree cascade — and registers itself in the fleet's shared registry.
It snapshots the parent's whole lexical scope (prelude, agent library, every
accumulated binding), its dynamic context (cwd, env, grants, handlers), and the
installed builtin table, and starts fresh in everything else — fresh control
counters and a freshly-defaulted `SessionState`, so it holds **no terminal
authority** (`TerminalAccess::Denied`, no lease — a sub-agent is not the
foreground agent and can never seize the controlling terminal the TUI owns).
There is no flow-back: the child's `cd`, env, and new bindings die with it. The
spawn tree is **unbounded in depth** — every agent may spawn — and mirrors on the
bus as `Kind::Born` / `Kind::Died`.

`fork_remembering` is the anamnesis variant: it uses the same shell/provider fork,
then asks `AgentLog` to import the parent's model-visible context and append the
tool call's prompt as the child's fresh final user prompt. `AgentLog` drops a
pending unanswered assistant tool-call frame when the parent is mid-dispatch, so
the child inherits a request context rather than a dangling provider protocol.
The agraphos path uses plain `fork` and seeds only the launch prompt into the
child's inbox.

Routing the fork through core matters because the builtin table is the easiest
thing to drop. The exarch host builtins — `window-hash`, `grep-files`, `edit`,
`explore-dir`, `line-hash` ([[map/exarch/shell-eval|agent_builtins]]) — live in
the agent's dispatch table, *outside* `Mobile`, and the `view-text` /
`view-text-around` helpers in `agent.ral` call `window-hash`. A fork that copied
only `mobile.scope` and `mobile.context` would leave the child's `view-text`
resolving to nothing and falling through to a failed PATH lookup. `fork_session`
copies `agent.builtins` as part of the flow matrix, so the decision lives in one
place and the table cannot be silently severed at this call site.

`digest.rs` holds `cap_and_spill` and the fixed byte caps for what the *model*
sees in history: the four tool-result sections (stdout/stderr/value/audit) share
`TOOL_RESULT_CAP` (~10 KiB, halved into a head and tail digest), alongside
separate caps for `fff` results, opaque error blobs, agent replies, and the
history-compaction threshold. Oversize sections spill to the session dir under a
content-hashed name the model can `head` / `tail` / `rg`. The user always sees
the full text live; caps only shape the model's view. `run_shell` here threads to
[[map/exarch/shell-eval|shell-eval]]; cancellation is the task-level `cancel` flag.

## See also

[[design/agents|agents]] (the role model these nodes realise — one `parent`
predicate, the conversing trunk vs returning agents),
[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]] (the Fleet/Agent
split this page maps), [[map/exarch/tools|tools]] (the registry and gates the
provider sees), [[map/exarch/frontend|frontend]] (the bus, the inbox, the
registry, and the two frontends), [[map/exarch/provider|provider]],
[[map/exarch/policy|policy]], [[map/exarch|exarch]].
