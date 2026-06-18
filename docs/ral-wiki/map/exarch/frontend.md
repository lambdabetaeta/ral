---
generated_at_commit: d8dbd81
generated_at_date: 2026-06-18
covers_paths: [exarch/src/bus.rs, exarch/src/event.rs, exarch/src/tui.rs, exarch/src/tui/, exarch/src/headless.rs, exarch/src/cancel.rs, exarch/src/host.rs]
---

# Map: exarch / frontend

The agent core and the user interface meet at **one event stream plus one
prompt queue**, defined by `bus.rs`:

- workers stamp a `Kind` with a `SessionId` through an `Emitter`; a `Sink`
  consumes them. A `Kind` is a token, boundary, usage, tool call/result,
  sub-agent lifecycle, a transient `Phase` label naming the worker's current
  synchronous op (shown beside the spinner, recorded to `events.json`), or one
  of the rail-decoration `Patch` / `Wrote` / `Task` / `Meter` variants a kit
  raises through the `surface` builtin — decoded onto the bus by
  [[map/exarch/shell-eval|shell-eval]]'s host sink.
- `PromptQueue` is the narrow TUI→worker back-channel: `Sink::prompt_queue`
  hands `pump` a shared queue, `App::enqueue` pushes prompts typed while busy,
  and `Emitter::drain_prompt_queue` lets the root dispatch loop consume them at
  safe tool boundaries ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]]).
- `pump` runs the worker on a scoped thread, drains the event channel into the
  sink, and reports a worker panic as a final `Kind::Error`.

Headless and test sinks expose an empty prompt queue; only the TUI supplies a
live producer.

`event.rs` is the canonical per-session record. `SessionLog` owns three things:

- the in-memory `Vec<SessionEvent>` — renders the next provider request, drives
  the protocol state machine (`is_ready` gates a fresh prompt and `quiesce` winds
  any in-flight turn back to it, so a turn never strands a prompt mid-protocol;
  [[invariants/turn-ends-ready|turn-ends-ready]]);
- a pretty-printed `events.json`, appended as each event lands — the post-mortem
  "model view";
- the spill directory, where oversize tool outputs land under a content-hashed
  name for [[map/exarch/session|`cap_and_spill`]].

The TUI writes a sibling `user.log` from the same stream — the "user view" —
flushed as each block lands so it survives an abnormal exit. Both files live
under the durable per-run log directory (`bootstrap::log_run_dir`,
`$XDG_STATE_HOME/exarch/<project>/<run>/sessions/<id>/`).

Two `Sink` implementations:

 `tui.rs` (+ `tui/{block,line,md,rail,viewport}.rs`) — the full-screen TUI.
 It owns the alternate screen and its own scrollback: each session is a
 `Vec<Block>` (`tui/block.rs`), and the whole frame is redrawn each tick from
 a memoised flatten of those blocks into wrapped visual rows. A tool call is
 the one collapsible block — its summary shows shut, the full ral script when
 a click opens it; the wheel scrolls, click-drag selects and copies the
 rail-stripped text via OSC-52, and Shift-drag falls through to the terminal's
 own selection. `tui/md.rs` is the streaming markdown renderer;
 `tui/viewport.rs` the per-session block buffer, scroll position, and
 `user.log` tee; `tui/rail.rs` the data-encoding marginal rail — one cell
 carrying three of Bertin's variables (shape → kind, hue → agent, value →
 magnitude), the keystone of the *transcript as graphic* re-encoding
 ([[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]],
 Phases 0–2 landed). The `rule_line` carries a value-ramp `ctx%` bar and a
 Gantt ribbon of completed phases; phase state lives on `Viewport`, not `App`.
 Sub-agent sessions get tabs that linger after `Died`, each keeping its own
 scroll position. It owns the REPL loop and the raw-mode / bracketed-paste /
 alt-screen / mouse-capture guard. A prompt the user submits while a turn
 runs is queued (`PromptQueue` behind `App::queue`); the root dispatch loop
 drains non-slash prompts at the next safe tool boundary, and `Repl::drive`
 drains any remainder as the next turn's prompt, coalesced oldest-first.
 Slash-prefixed prompts stay on the REPL command path. Until then the queue
 renders in a strip above the input
 ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]]).
- `headless.rs` — one-shot pipe: assistant tokens to stdout, every other event
  condensed to one line on stderr, exit after one seed turn. Takes the default
  `Sink::drive` and an empty prompt queue.

`cancel.rs` is the per-root-turn cancellation layered on ral's interrupt
handling. A `run_turn` mints a `Token` (an `Arc<AtomicBool>`) and threads it
through `apply` → dispatch → tools → child sessions; a sub-agent shares the
parent's token, so one Esc cancels the whole tree
([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]). The token's
flag is published into a lock-free `AtomicPtr` slot for the signal handler (a
handler must not lock); `is_set` reads the same slot, so the provider's
mid-stream cancel race — which holds no token — observes the same cancellation.
A key-driven Esc sets the current root token *and* drives ral's non-escalating
`process::interrupt`, so a single press both aborts the turn loop / in-flight
HTTP future and unwinds the in-flight eval at its next `signal::check` — and,
because that store never `fetch_add`s, repeated Esc cannot reach ral's
third-signal `_exit` ([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).
A genuine external signal still routes through ral's escalating handler.
Minting is the reset, so the flag never clears at every `apply`; exarch session
shells are rebuilt only through `bootstrap::boot_shell`, which discards stale ral
interrupts before library loading and returns with the cancel chain installed
over ral's handlers. `/clear` therefore works after Esc and SIGINT after
`/clear` still raises cancel. `host.rs` snapshots the machine (OS, date, cwd,
user, git state) once at startup for the [[map/exarch/policy|system prompt]].
