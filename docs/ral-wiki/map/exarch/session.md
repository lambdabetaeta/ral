---
generated_at_commit: e0e912dc
generated_at_date: 2026-06-17
covers_paths: [exarch/src/session.rs, exarch/src/nudge.rs, exarch/src/digest.rs]
---

# Map: exarch / session

`session.rs` is the turn driver. A `Session` owns one continuous exchange: the
canonical [[map/exarch/frontend|event log]] (`SessionLog`), the persistent
[[map/core/shell-state|`Shell`]], the session `Capabilities`, and the flags that
shape its turns (`is_subagent`, the headless-only `expect_action`, the per-turn
`acted`). Output caps are fixed `digest.rs` constants, not per-session state.
Three nested loops:

- `run_turn` — one user turn. Each pass runs `apply` on a scoped worker thread
  (via `bus::pump`) and hands the `TurnOutcome` to `nudge::Registry::react`,
  which decides whether to stop or loop with a (possibly synthetic) next prompt.
  Under the headless-only `expect_action` flag (root sessions only; forks never
  inherit it), a clean completion is gated by two one-shot, budget-free nudges
  before it is accepted as the result: a turn that never dispatched a tool earns
  an idle nudge to engage, and a turn that did earns a verify nudge to re-read its
  output against the task's stated requirements — a clean exit being evidence the
  command ran, not that the answer is correct.
  However the turn ends — completion, cancellation, or a surfaced provider error
  — its single exit returns the session `ReadyForUser`, winding a stranded prompt
  back through `quiesce` (the same recovery cancellation uses), so the next
  `append_user` is always admissible ([[invariants/turn-ends-ready|turn-ends-ready]]).
  On a caught worker panic (`pump` → `Ok(None)`) it also rebuilds the live shell's
  dynamic context from the `durable: Mobile` snapshot the worker refreshed at the
  last clean tool-call boundary, rolling the panicking call's grant/env/cwd/handler
  effects back while completed calls' bindings survive
  ([[decisions/260612_exarch-panic-recovery|panic-recovery]]; the IO half is
  shell-eval's `IoGuard`).
- `apply` — one provider round-trip loop: render the transcript, stream a reply
  through `provider.complete`, **admit** then append the assistant message,
  dispatch any tool calls, append their results, optionally append a drained
  steering prompt, repeat until the model emits no tool call. The admission step
  (`admit_assistant`, run at the commit boundary) enforces the
  [[invariants/transcript-admission|transcript-admission invariant]]: it repairs a
  non-object tool-call `fn_arguments` to `{}` (X2) and substitutes a stub for an
  otherwise-empty assistant message (X7), so every committed message serialises
  to a request a strict backend accepts. `StopReason::MaxTokens` *with no
  captured tool call* raises `ProviderError::Truncated` after appending the
  partial, so a `continue` nudge keeps the work as context; *with* captured tool
  calls it dispatches them and continues the loop instead, since returning
  `Truncated` there would strand the protocol in `AwaitingToolResults` and fail
  the nudge's next `append_user` (X6). A hard `MAX_STEPS` ceiling (250) ends a
  turn whose model never stops calling tools — the headless/autonomous
  counterpart to interactive Esc — returning `TurnOutcome::Capped`. That outcome
  matches no nudge rule, so the driver treats it as terminal; re-driving would
  only spend the ceiling again.
- `dispatch` — runs the turn's tool-call batch under one `thread::scope`. Each
  call is staged first; `agent` calls return `Staged::Spawned`, so same-batch
  sub-agents can overlap before the join phase. Once every requested tool id has
  a result, dispatch drains the root prompt queue. A non-slash queued prompt is
  appended after the complete tool-result batch, and the next loop asks the
  provider with the user's steering in context
  ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]]). Sub-agents
  do not consume the root prompt queue.

`clear` resets the root session without carrying cancellation residue forward:
it obtains a fresh root shell from `boot_root_shell` (the scratch-seeding wrapper
around `bootstrap::boot_shell`, where stale-interrupt discard and cancel
re-chaining live), truncates and restarts the event log with a fresh
`SessionStarted`, and installs `EXARCH_SESSION_DIR` through the same shell
replacement path used by construction.

`compact` runs `provider.summarize` over the history when it crosses
`COMPACT_THRESHOLD` (`digest.rs`, 500 KiB) and `SessionLog::can_compact` holds
(no pending tool results). It is called at the **top of `apply`**, where the
session is `ReadyForUser` ([[invariants/turn-ends-ready|turn-ends-ready]]) and
the gate actually holds — every provider round-trip (each user turn, each nudge
iteration) passes through here, so long autonomous and headless turns stay
bounded without an interactive `/compact`. The prior placement sat after
appending tool results, in `AwaitingAssistantAfterToolResults`, where
`can_compact()` is always false, so auto-compaction never fired (X1). A
turn-boundary Esc bails before the summarize request
([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).

`fork` value-snapshots the parent's shell context (cwd, env, definitions) into a
child `Session` for the `agent` tool; mutations do not propagate back. The call
tree lives on the Rust call stack and mirrors as `Kind::Born` / `Kind::Died` on
the bus.

`digest.rs` holds `cap_and_spill` and the fixed byte caps for what the *model*
sees in history: the four tool-result sections (stdout/stderr/value/audit) share
`TOOL_RESULT_CAP` (~10 KiB, halved into a head and tail digest), alongside
separate caps for `fff` results, opaque error blobs, agent replies, and the
history-compaction threshold. Oversize sections spill to the session dir under a
content-hashed name the model can `head` / `tail` / `rg`. The user always sees
the full text live; caps only shape the model's view. `run_shell` here threads to
[[map/exarch/shell-eval|shell-eval]]; cancellation is the task-level `cancel` flag.
