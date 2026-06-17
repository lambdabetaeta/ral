---
status: active
---

# Tool-boundary steering for the prompt queue

A queued prompt belongs to the running turn as soon as the tool protocol reaches a safe boundary.

## The decision

**The root TUI prompt queue drains at whole tool-call batch boundaries before
the next assistant step, not inside the provider's pending `tool_use` block.**

- `PromptQueue` is a shared `bus` object: `Tui::prompt_queue` gives `pump` the same queue `App::enqueue` writes and the pending strip renders. Default sinks expose an empty queue.
- `Session::dispatch` stages the whole assistant tool-call batch under one
  `thread::scope`. Same-batch `agent` calls spawn before the join phase, so
  sibling agents overlap.
- After every assistant-requested tool id has a real or cancelled `ToolResult`,
  dispatch drains the root prompt queue. A drained prompt is appended as a
  model-visible user message before the next provider request.
- Sub-agents do not consume the root queue.
- Any prompt not drained mid-turn still flows through `Repl::drive` at the ordinary turn boundary, coalesced oldest-first.
- Slash-prefixed prompts remain queued for `Repl::drive`; frontend commands such
  as `/clear` are not consumed as steering.

## Why this shape

The queue may steer work already in motion without breaking provider protocol or
serialising sibling agents:

- tool-call protocol stays closed: every assistant-requested tool id gets a `ToolResult` before any user prompt follows;
- same-batch parallelism is preserved for forking tools such as `agent`;
- queued input redirects the next assistant step, not tool calls the model
  already issued in the current batch;
- the pending strip is faithful because the worker drains the same queue the UI displays;
- the turn still exits through `ReadyForUser` ([[invariants/turn-ends-ready|turn-ends-ready]]), so nudge and compaction gates keep their old shape.

## Boundary

The boundary is the assistant's whole pending tool-call batch. Draining earlier
would let a queued prompt stop later calls in the same batch, but that would also
serialise same-batch `agent` calls. The protocol does not require that: it only
requires every assistant-requested tool id to receive a `ToolResult` before any
user message is appended.

So dispatch stages the whole batch, lets same-batch tools overlap where their
tool implementation permits it, appends every result, then appends the queued
prompt before the next provider request. The tradeoff is explicit: queued user
input no longer prevents later calls in the already-issued batch from running;
it steers the next assistant step.

## Where

- **`exarch/src/bus.rs`** — `PromptQueue`, `Sink::prompt_queue`, and `Emitter::drain_prompt_queue` form the frontend→worker back-channel.
- **`exarch/src/tui.rs`** — `App::queue`, `App::enqueue` / `App::take_queue`, the busy Enter arm, and the pending strip.
- **`exarch/src/event.rs`** — `SessionLog::append_steering` admits a user message only after a complete tool-result batch.
- **`exarch/src/session.rs`** — `dispatch` stages the whole batch, joins `Staged::Spawned` before the next provider request, and appends drained steering after the complete result batch.

## Covered

- `session_apply::queued_prompt_steers_after_current_tool_batch` — a prompt queued while the first tool sleeps is committed before the next assistant step; the second already-issued tool call still runs.
- `bus::tests::prompt_queue_steering_drain_stops_before_slash_command` — slash
  commands stay queued for the REPL path while earlier steering text drains.
- `tui::tests::enqueue_coalesces_in_order_then_drains_empty` and `tui::line::queued_prompt_*` — the pending strip and boundary coalescing still work for prompts not drained mid-turn.

## The hard rule

A queued prompt reaches the model at the first safe boundary: after all currently pending tool ids have results. It is never inserted between an assistant tool-call message and the required tool responses, and slash-prefixed prompts are left to the REPL command path.

See also [[decisions/260613_prompt-queue|prompt-queue]], [[map/exarch/frontend|frontend]], and [[map/exarch/session|session]].
