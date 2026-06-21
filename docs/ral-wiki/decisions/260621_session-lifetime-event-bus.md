---
status: active
---

# A live background agent needs a session-lifetime event bus

**An async `agent` is muted only because the event bus is per-turn; lifting
the bus to session lifetime lets a background child stream a live tab exactly
like a sync child, and it is a *pure presentation change* because completion is
already a control-flow fact, not a bus state.** The async/sync split is a real
*dependency-vs-orchestration* distinction in what the model sees
([[decisions/260617_async-agent-tool|async-agent-tool]]); the live-tab gap is
*not* part of that distinction — it is an artifact of the channel's lifetime,
and it is the only asymmetry left once the settle breadcrumb is unified.

## Context

A sub-agent reaches the frontend over **two** independent channels, and the two
modes use different ones:

- **The bus** — `Kind` events through an `Emitter`, drained by `Sink::drive` /
  `drive_events`. It is **per-turn**: `pump` mints a fresh `channel()` inside its
  `thread::scope` and closes it when the turn's `done` flag is set
  (`exarch/src/bus.rs`). It carries `Born`/`Token`/`ToolCall`/`Died` and drives
  the live *viewports* and *tabs*.
- **The inbox** — `InboxMsg` through a session-lived `Arc<Mutex<…>>`, drained at
  the turn boundary ([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]).
  It carries the settle digest `InboxMsg::AgentResult`.

A **sync** child rides the bus, so it gets a live tab. An **async** child is a
detached registry-owned worker that outlives the spawning turn
([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]),
so it *cannot hold the turn's sender* — the channel is gone before it finishes.
It is therefore muted to its own `SessionLog` and surfaces only through the
inbox, as the deferred `Turn::Agent` breadcrumb. This is the same denial
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] makes: a detached
writer may hold only root- or handle-owned resources, never a foreground frame's
capture buffer.

Two facts narrow the change:

- **The settle breadcrumb is already one path.** Both modes reduce a child's
  `TurnOutcome` through the single `AgentOutcome::breadcrumb` and draw it with the
  one `push_subagent` via `Kind::SubagentDone`. So this decision is *not* about
  drawing results — that is done — it is only about **live streaming while the
  child runs**.
- **The frontend is already concurrency-ready.** Every `Event` is
  `SessionId`-stamped, and the TUI routes *all* of it by id into per-session
  `Viewport`s and tabs (`Born`/`Token`/`Died` in `exarch/src/tui.rs`). The sink
  state that a tab needs — `viewports`, `tabs`, `dispatch_order`, the
  `dying`/`LINGER` age-out, `AGENT_HUES` — all lives in `App` across turns.
  Nothing there assumes a single active worker. The *only* turn-scoped thing is
  the channel.

So the live-tab gap reduces to one seam: a worker that outlives a turn has no
worker→frontend channel that also outlives the turn.

## Decision

- **Mint the bus once per session, not once per turn.** The `(tx, rx)` is
  created at REPL start and owned for the session's life. `pump` keeps the
  per-turn `done` flag and the foreground worker thread, but no longer owns the
  channel. Every worker — the foreground turn *and* each detached async child —
  receives an `Emitter` cloned off the session sender, stamped with its own id.

- **The async path swaps its muted emitter for a real one.** It emits `Born` /
  `Token` / `ToolCall` / `Died` like any child and so gets a tab through the
  *existing* draw path — no new rendering code. The muting in
  `dispatch_async` is deleted; nothing else in the async dispatcher changes.

- **Completion stays a control-flow fact, and the drain honours it independent
  of bus state.** The foreground turn still ends on its explicit per-turn `done`,
  never on the channel emptying or disconnecting — the
  [[decisions/260618_run-turn-host-loop|run-turn-host-loop]] invariant that the
  bus is presentation, not liveness. The refinement: `drain_pass` /
  `drive_events` must check `done` *even while the channel is non-empty*, because
  concurrent background producers keep it non-empty. On `done`, drain the
  buffered batch, render a final frame, and return; any in-flight background
  events flow to the idle drainer below. This *upholds* the invariant — `done`
  remains the sole arbiter — and removes the latent conflation of "channel empty"
  with "turn over" that muting currently hides.

- **The idle wait gains the bus as a third source.** The select that
  [[decisions/260617_scheduled-wakeups|scheduled-wakeups]] made `{input, inbox}`
  (`read_prompt`, `Idle::{Prompt, Inbox, Quit}`) becomes `{input, inbox, bus}`:
  at rest, the frontend drains the session bus so background tabs advance and a
  child's `Died` ages its tab out while the user sits at the prompt.

- **Worker lifetime and the model-facing contract are untouched.** The child
  still owns its `Arc<Provider>` and cancel `Token`, still sits in the ephemeral
  registry with a `generation` and the `process::reaper` ceiling. Its reply still
  returns *only* through the inbox as a deferred, marked `Turn::Agent` — the
  orchestration edge is unchanged. **This decision adds a live frontend view; it
  changes nothing the model sees.**

- **Tabs age out under `/clear` by generation, not by muting.** Today muting
  means a cleared worker leaves no live tab to retract. With live tabs,
  `AgentRegistry::clear` (which cancels workers and bumps the generation) must
  also drive the frontend to age out those tabs, so a worker cancelled across a
  context rebuild does not keep painting into a session that no longer expects
  it — the visual twin of the stale-generation rejection the inbox already does.

- **This is a TUI property, like the `watch` durable-sink axis.** Headless has no
  idle loop and no tabs; it keeps muting async children, with the stderr
  `SubagentDone` breadcrumb as their only trace. Bus lifetime is a host capability
  ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]'s axis), not a
  core obligation.

## Why this shape

- **It removes an incidental asymmetry while preserving the essential one.** The
  dependency-vs-orchestration edge — what the *model* receives — is untouched.
  Only the *frontend's* view converges, and converging it costs almost nothing
  because the draw path is already shared and id-routed.

- **It rests on an invariant already in force.** Once
  [[decisions/260618_run-turn-host-loop|run-turn-host-loop]] made completion a
  control-flow fact and the bus presentation, the per-turn channel became the
  *conservative default*, not a load-bearing boundary. Session lifetime is its
  natural relaxation; the deferral in
  [[decisions/260617_async-agent-tool|async-agent-tool]] ("lift the bus for live
  background output now — deferred") is exactly the question this answers.

- **The risk is localised.** Three sites move — channel lifetime, the
  completion-drain refinement, the idle select — and only the second carries
  subtlety. Everything visual (viewports, hues, `LINGER`, the unified breadcrumb)
  is reused unchanged.

## Alternatives considered

- **Stay muted (status quo).** The `agents` listing plus the settle breadcrumb.
  Cheapest, and correct if *watching* background work is not wanted. Rejected only
  if live visibility is a goal; it loses nothing model-facing.

- **Per-worker channels selected in the frontend.** A dynamic set of receivers
  merged at the TUI. `mpsc` has no native select, so this converges to a single
  shared sender — which *is* the session bus — with strictly more plumbing.
  Rejected as the same design wearing more machinery.

- **Tail each child's `SessionLog` into a tab.** Reuses the log the child already
  writes, sidestepping the channel entirely. Rejected: it creates a second
  rendering path (log-replay vs event-stream) and two sources of truth per tab.
  The bus is the honest live source; the log is the durable record, not a UI feed.

- **Lift the bus but keep completion gated on an empty channel.** Rejected: with
  concurrent background producers the channel is never momentarily empty, so the
  foreground turn would fail to terminate. Completion must remain the explicit
  `done` fact, per [[decisions/260618_run-turn-host-loop|run-turn-host-loop]].

## Open questions

- **Tab-bar pressure.** Steady-state *N* concurrent background tabs is a new
  regime; sync tabs are short-lived within one turn. The
  [[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]
  agents×steps matrix is the likely home — background agents as a compact strip
  unless focused — but the policy is open.

- **`/clear` teardown affordance.** Whether a live background tab retracts
  visibly on clear or simply ages out through the existing `dying`/`LINGER` path.

- **Headless visibility.** Whether a headless background agent ever surfaces more
  than its settle breadcrumb (a periodic stderr heartbeat), or stays settle-only
  because the host has no live surface.

- **Surfacing from a background child.** A live background worker that raises a
  `Card` through `surface` competes for the foreground; the interaction with
  [[decisions/260619_terminal-lease|terminal-lease]] / terminal-foreground
  ownership is unexplored, since muting currently forecloses it.

## As built

Implemented as specified. The seam is a `SessionBus` value (`bus.rs`) owning the
channel and inbox, *borrowed* by `pump`/`run_turn`: `SessionBus::session` for the
TUI (emitters session-lived, async children stream a live tab),
`SessionBus::per_turn` for headless and tests (async children muted). An
`Emitter::session_lived` flag, inherited by `child`, is what `dispatch_async`
reads to choose a streaming vs muted child emitter, bracketing `run_child` with
`Born`/`Died`. `drain_pass` latches `done` at the top of each pass and stops on
it regardless of channel occupancy; the idle wait gained `drain_bus_idle` as a
third source; `App::clear` retires live tabs into the `dying`/`LINGER` path and an
`App::handle` dying-guard drops a cancelled worker's late events. Headless and the
model-facing inbox contract are unchanged.

## See also

[[decisions/260617_async-agent-tool|async-agent-tool]] (the mode split this
completes; its deferred "lift the bus" open question),
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]] (completion as a
control-flow fact; the bus as presentation, not liveness — the invariant this
rests on),
[[decisions/260618_host-seam-turn-observer|host-seam-turn-observer]] (the
superseded attempt that motivated that invariant),
[[decisions/260617_scheduled-wakeups|scheduled-wakeups]] (the inbox seam and the
`{input, inbox}` idle select this extends with the bus),
[[decisions/260616_tool-boundary-steering|tool-boundary-steering]] (why the
inbox delivery is not steering),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (the durable-sink host
axis bus lifetime mirrors),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the detached-worker mechanics left untouched),
[[invariants/turn-ends-ready|turn-ends-ready]],
[[internals/a-turn-end-to-end|a-turn-end-to-end]],
[[internals/output-capture-and-detachment|output-capture-and-detachment]],
[[map/exarch/frontend|frontend]], [[map/exarch/session|session]],
[[map/exarch/tools|tools]].
