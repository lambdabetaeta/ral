---
status: active
verified_at_commit: c848c533
verified_at_date: 2026-10-01
anchors: [drain_mid_exchange, next_or_idle, append_steering, append_tool_results]
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
- Any prompt not drained mid-turn still reaches the model at the exchange boundary (`Inbox::next_or_idle`), coalesced oldest-first.
- A session *read* (`/branch`, `/context`, `/resources`) drains at the same
  tool boundary and runs at its place in the typed order, each delivery
  around it landing as a steering line of its own; a *rewrite* (`/clear`, `/evict`, `/rewind`, `/quit`) waits for the exchange
  boundary and holds everything typed after it. The three command classes
  are the [[map/exarch/frontend|frontend]] page's.

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

- **`exarch/src/bus/inbox.rs`** — `drain_mid_exchange` (the tool-boundary drain: reads and deliveries in typed order, stopping at a rewrite) and `next_or_idle` (the exchange boundary).
- **`exarch/src/agent/event.rs`** — `append_steering` admits a user message only while an assistant reply is awaited, one record per message.
- **`exarch/src/agent/deliberate.rs`** — after `append_tool_results`, walks the drained arrivals in order: a read runs, a delivery is appended as steering.

## Covered

- `deliberate::tests::a_read_queued_mid_exchange_runs_at_the_tool_boundary_ahead_of_the_next_step` — a `/resources` and a prompt queued from inside a batch: the fold precedes the next assistant step, and the prompt lands as steering at that boundary.
- `deliberate::tests::a_read_queued_after_a_prompt_runs_after_it_lands` — the reverse order holds too: the arrivals are taken as typed.
- `deliberate::tests::a_prompt_queued_across_an_interrupt_opens_the_next_exchange_over_the_whole_context` — a cancelled batch admits nothing.
- `bus::inbox::tests::inbox_mid_exchange_drain_takes_reads_in_order_and_holds_at_a_rewrite`.

## The hard rule

A queued prompt reaches the model at the first safe boundary: after all currently pending tool ids have results. It is never inserted between an assistant tool-call message and the required tool responses. A slash-shaped line that names no command is prompt text like any other.

See also [[map/exarch/frontend|frontend]] and [[map/exarch/agent|agent]].
