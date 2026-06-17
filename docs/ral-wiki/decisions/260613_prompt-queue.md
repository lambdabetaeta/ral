---
status: superseded
superseded_by: decisions/260616_tool-boundary-steering
---

# A prompt queue dispatched at the turn boundary

> Superseded by [[decisions/260616_tool-boundary-steering|tool-boundary steering]].

The TUI REPL was strictly sequential: read one prompt, run one turn, read
the next. While a turn was in flight `drive_events` let the user *compose* the
next prompt, but Enter only inserted a newline — the typed text sat in the
editor until the turn ended and the user pressed Enter a second time to send
it. A follow-up thought formed mid-turn could not reach the model without that
second keypress.

## The decision

**A prompt the user submits while a turn runs is queued and delivered as the
next turn's prompt the moment the current turn ends — no second keypress.**

- Enter on the main tab during a turn calls `App::enqueue`, which runs the
  ordinary `submit` (trim, history, clear the editor) and pushes the result
  onto `App::queue` rather than running it. Off-main tabs stay watch-only;
  Shift/Alt-Enter still inserts a newline for a multi-line draft.
- `Repl::drive` reads its next prompt from three sources in order: the seed,
  then `App::take_queue`, then a blocking `read_prompt`. `take_queue`
  coalesces the queued prompts oldest-first, joined by a blank line, into one
  prompt; only an empty queue falls through to the blocking read.
- The coalesced prompt flows through `handle_slash` like any typed one, so it
  echoes as it is sent and a lone queued `/clear` still works.
- While they wait, the queued messages render in a pending-prompt strip above
  the input (`line::queued_prompt`), on a pink `↩` rail distinct from the cyan
  `❖` reverse-video echo of a sent prompt, capped at a third of the screen.

## Why the turn boundary, not mid-turn injection

The agent/frontend channel is one-way by construction: workers stamp events
through an `Emitter`, a `Sink` consumes them, and a frontend never reaches back
into the agent ([[map/exarch/frontend|frontend]]). Delivering a queued message
*into* a running turn would need a new frontend→worker back-channel and would
reshape the nudge / turn-end protocol. Boundary dispatch keeps the one-way
boundary intact, and "as soon as possible" is precisely the next
`ReadyForUser` — which [[invariants/turn-ends-ready|turn-ends-ready]]
guarantees every turn reaches, however it ends. Coalescing rather than running
one turn per fragment lets the model read a burst of related follow-ups as a
single message.

## Where

- **`exarch/src/tui.rs`** — `App::queue`, `App::enqueue` / `App::take_queue`;
  the `drive_events` Enter arm that enqueues on the main tab; `Repl::drive`'s
  three-source prompt order; the `draw` strip above the prompt row.
- **`exarch/src/tui/line.rs`** — `queued_prompt`, the pending-strip builder.

## Covered

- `tui::tests::enqueue_coalesces_in_order_then_drains_empty` — submission order
  is preserved, an empty draft queues nothing, and draining empties the queue.
- `tui::line::tests::queued_prompt_marks_each_message_and_wraps` /
  `queued_prompt_truncates_with_remainder` — the strip marks and wraps each
  message and closes a long queue with a `⋯ (N more)` line.

## The hard rule

A queued prompt reaches the model only at a turn boundary, through the REPL's
ordinary prompt path — never injected into a live turn. The frontend stays a
pure consumer of the event stream.

See also [[map/exarch/frontend|frontend]], [[map/exarch/session|session]], and
[[invariants/turn-ends-ready|turn-ends-ready]].

