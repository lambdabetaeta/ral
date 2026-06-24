---
generated_at_commit: 1baac6d
generated_at_date: 2026-06-22
covers_paths: [exarch/src/bus.rs, exarch/src/event.rs, exarch/src/tui.rs, exarch/src/tui/, exarch/src/headless.rs, exarch/src/cancel.rs, exarch/src/host.rs]
---

# Map: exarch / frontend

The agent core and the user interface meet at **one outbound event stream and
one inbound inbox**, defined by `bus.rs`:

- workers stamp a `Kind` with a `SessionId` through an `Emitter`; a `Sink`
  consumes them. A `Kind` is a token, boundary, usage, tool call/result,
  sub-agent lifecycle, a transient `Phase` label naming the worker's current
  synchronous op (shown beside the spinner, recorded to `events.json`), or a
  `Card` — a render document a kit raises through the `surface` builtin, decoded
  onto the bus by [[map/exarch/shell-eval|shell-eval]]'s host sink and drawn by
  one generic interpreter ([[map/exarch/cards|cards]]).
- `Inbox` is the typed inbound twin — a per-session queue of `InboxMsg`s, each
  carrying its source and drain boundary. User steering drains mid-turn at a
  tool boundary (`drain_tool`); a scheduled wakeup or a settled async agent
  drains at the turn boundary as its own marked `Turn` (`drain_turn`)
  ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]],
  [[decisions/260617_scheduled-wakeups|scheduled-wakeups]],
  [[decisions/260617_async-agent-tool|async-agent-tool]]).
- a `SessionBus` owns the event channel and the inbox; `pump` borrows it, runs
  the worker on a scoped thread, drains events into the sink, and reports a
  worker panic as a final `Kind::Error`. Completion is the per-turn `done` flag,
  latched by `drain_pass` so a turn ends even while a background producer keeps
  the channel non-empty — never the channel's state
  ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]).

The TUI mints one **session-lived** bus, so a detached async agent clones its
sender and streams a live tab through the same id-routed draw path a sync child
uses ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]);
headless and test sinks build a **per-turn** bus that closes when the worker
finishes, keeping async children muted to their own log.

`event.rs` is the canonical per-session record. `SessionLog` owns three things:

- the in-memory `Vec<SessionEvent>` — renders the next provider request, drives
  the protocol state machine (`is_ready` gates a fresh prompt and `quiesce` winds
  any in-flight turn back to it, so a turn never strands a prompt mid-protocol;
  [[invariants/turn-ends-ready|turn-ends-ready]]);
- a pretty-printed `events.json`, appended as each event lands — the post-mortem
  "model view";
- the spill directory, where oversize tool outputs land under a content-hashed
  name for [[map/exarch/agent|`cap_and_spill`]].

The TUI writes a sibling `user.log` from the same stream — the "user view" —
flushed as each block lands so it survives an abnormal exit. Both files live
under the durable per-run log directory (`bootstrap::log_run_dir`,
`$XDG_STATE_HOME/exarch/<project>/<run>/sessions/<id>/`). Every touch of that
file lives in one place: `tui/viewport.rs` keeps both the tee writer
(`open_log`) and the `/export` copy (`export_log`) beside each other, the
single `user.log` I/O door, so `Repl::cmd_export` resolves and guards the
destination but never reaches the filesystem itself.

Two `Sink` implementations:

 `tui.rs` (+ `tui/{block,group,line,md,rail,viewport}.rs`) — the full-screen
 TUI. It owns the alternate screen and its own scrollback: each session is a
 `Vec<Block>` (`tui/block.rs`), and the whole frame is redrawn each tick from
 a memoised flatten of those blocks into wrapped visual rows. A tool call is
 the one collapsible block — its summary shows shut, the full ral script when
 a click opens it; the wheel scrolls, click-drag selects and copies the
 rail-stripped text via OSC-52, and Shift-drag falls through to the terminal's
 own selection. `tui/md.rs` is the streaming markdown renderer; `tui/group.rs`
 the coalescing projection that folds an observation run into one dialable
 object; `tui/viewport.rs` the per-session block buffer, scroll position, and
 `user.log` writer; `tui/rail.rs` the data-encoding marginal rail. The
 transcript is laid out as a graphic on two orthogonal planes
 ([[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]],
 Phases 0–7 landed):

 - **two voices, encoded as foreground and background.** *The transcript
   collapses to two parties — the human and the agent field — read off
   orthogonal channels so neither competes with the other.* The agent owns the
   chromatic foreground (the rail hues); the background plane carries one
   distinction only — machine text. A run of script or observation output is
   washed into a recessed `CODE_BG` panel (`group::wash_inset`): a *left-inset*
   rectangle whose left edge aligns with the content — so it nests under its
   intent, and script and output share one margin to read as a single region —
   and whose right edge still runs to the margin, so the wash reads as a stratum
   rather than a content-hugging swatch; model prose sits unwashed at
   the base; the human's submitted prompt is the sole occupant of a *third*
   register, a raised cool `PROMPT_BG`-banded block opened by a full-width
   `PROMPT_INK` rule fence (`line::prompt_fence`), found at a glance by common
   region rather than by reverse video, which stays reserved for an active
   selection.
 - **figure and ground, on the luminance axis.** Within the agent's
   foreground, *communication* and *work* split by value: the model's prose
   answer, a subagent's returned result, and the human's prompt stay at full
   luminance (the figure); a tool call's *intent* — work-narration, not the
   answer — drops to the `SLATE` ground tier (`group.rs` intent ink,
   `line::tool_call_header`), joining the already-recessed machine output and
   widening the app's `DIM` idiom from "minor chrome" to "the ground stratum".
   The rail glyph and the size/sparkline bars keep their luminance — there it
   is *magnitude* — so figure/ground rides value on content marks only and
   never collides with the quantitative read.
 - **the marginal rail: one cell, three variables.** Shape → block *kind*, hue
   → the *producing agent*, value → *magnitude*. Hue is a per-*view* tint, not
   a per-block one: every block in a tab shares that tab's agent slot
   (`Viewport::agent`, threaded into `Block::lines` at render time), so the
   whole rail glows one hue, read on a tab-switch as "whose transcript is
   this". The human's prompt fence is the lone exception — a `❖` in neutral
   `PROMPT_INK` so it never reads as just another agent's mark.
 - **the in-flight reply as a growing magnitude.** Streamed-but-uncommitted
   assistant text never paints as prose: `Viewport::streaming_seat` projects
   the open buffer as a single trailing row — the markdown rail glyph plus a
   `size_bar` of its line count — that grows in place as one extra scroll row,
   so the settled transcript above stays a finished image until a fence-safe
   break commits the real `Block::markdown`.
 - **a surfaced general card as a bounded object.** A diff-less
   `CardOrigin::Surfaced` card — the model's deliberate "look at this" —
   renders through `line::render_card_framed` as an indented framed box, its
   heading lifted into the top rule, no marginal rail glyph (the frame is its
   mark). A file mutation — a diff card or a write card — wears the
   patch-shape change-bar `▎`; an observation card folds into its ral group.

 The frame's terminal writes are bracketed in a synchronized update
 (`BeginSynchronizedUpdate` / `EndSynchronizedUpdate`) so the emulator swaps
 the whole diff atomically — without it a tail-following redraw tears while a
 full page streams tool calls. The same steadiness is held in the scroll
 arithmetic: `Viewport::render_window` head-anchors the trailing live segment
 (`TailAnchor` pins its head row at the greatest height it has reached), so a
 burst of streaming calls coalesces into one group whose churn opens a
 transient gap below rather than shoving the committed transcript up and down.
 The scrollbar reads true because `render_window` maps `offset` (first visible
 row, topping at `total - height`) onto ratatui's `[0, total-1]` cursor range
 and clamps it (`scrollbar_pos`), so the thumb actually reaches the bottom.

 The `rule_line` carries a value-ramp `ctx%` bar and an elapsed-wait bar
 (elapsed wall-time on the live phase, resetting per round-trip); the live
 phase lives on `Viewport`, not `App`.
 Sub-agent sessions get tabs that linger after `Died`, each keeping its own
 scroll position; an async agent on the session-lived bus streams its tab the
 same way, and `/clear` retires every live sub-tab through the same linger
 window ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).
 It owns the REPL loop and the raw-mode / bracketed-paste /
 alt-screen / mouse-capture guard. A prompt the user submits while a turn runs
 is posted to the `Inbox`; the root dispatch loop drains non-slash steering at
 the next safe tool boundary, and `Repl::drive` delivers the rest at the turn
 boundary — a coalesced human run, or a wakeup / settled agent as its own
 marked turn. A committed human turn echoes on the `RailShape::Prompt` band;
 a wakeup stays dim, ambient chrome with no rail glyph (`RailShape::Plain`).
 Slash-prefixed prompts
 stay on the REPL command path. Until then the inbox renders as a `PROMPT_BG`
 strip above the input, and the idle wait selects over input, inbox, and the
 session bus
 ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]],
 [[decisions/260617_scheduled-wakeups|scheduled-wakeups]]).
- `headless.rs` — one-shot pipe: assistant tokens to stdout, every other event
  condensed to one line on stderr, exit after one seed turn. Takes the default
  `Sink::drive` and a per-turn bus, so its async children stay muted.

`cancel.rs` is the per-root-turn cancellation layered on ral's interrupt
handling. A `run_turn` mints a `Token` (an `Arc<AtomicBool>`) and threads it
through `apply` → dispatch → tools → child sessions; a sub-agent shares the
parent's token, so one active-turn Ctrl-C or Esc cancels the whole tree
([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]). The token's
flag is published into a lock-free `AtomicPtr` slot for the signal handler (a
handler must not lock); `is_set` reads the same slot, so the provider's
mid-stream cancel race — which holds no token — observes the same cancellation.
The TUI key table keeps UI control separate from cancellation: idle Ctrl-C/Ctrl-D
quit, overlays close, and only active-turn Ctrl-C/Esc set the root token and
drive ral's non-escalating foreground cancel. A single press stops the turn loop /
in-flight HTTP future and unwinds the in-flight eval at its next `signal::check`;
because the path never `fetch_add`s, repeated presses cannot reach ral's
third-signal `_exit` ([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).
A genuine external signal still routes through ral's escalating handler.
Minting is the reset, so the flag never clears at every `apply`; exarch session
shells are rebuilt only through `bootstrap::boot_shell`, which discards stale ral
interrupts before library loading and returns with the cancel chain installed
over ral's handlers. `/clear` therefore works after Esc and SIGINT after
`/clear` still raises cancel. `host.rs` snapshots the machine (OS, date, cwd,
user, git state) once at startup for the [[map/exarch/policy|system prompt]].
