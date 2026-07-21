---
generated_at_commit: caa853f2ddce6abeeafda69239711a1b284706ca
generated_at_date: 2026-07-21
covers_paths: [exarch/src/bus.rs, exarch/src/agent/event.rs, exarch/src/tui.rs, exarch/src/tui/, exarch/src/headless.rs, exarch/src/agent/cancel.rs, exarch/src/prompt/host.rs]
---

# Map: exarch / frontend

The agent core and the user interface meet at **one outbound event stream and
one inbound inbox**, defined by `bus.rs`:

- workers stamp a `Kind` with an `AgentId` through an `Emitter`; a `Sink`
  consumes them. A `Kind` is a token or reasoning run (a streamed `Thinking`
  delta, committed by a final `Reasoning` event), boundary, usage, tool or
  harness call/result,
  sub-agent lifecycle, a transient `Phase` label naming the worker's current
  synchronous op (shown beside the spinner, recorded to `events.json`), or a
  decoded surface class — a `Card` render document a kit raises through the
  `surface` builtin, a structural `Io` event, a housekeeping `Notice`, a
  `Pin`/`Unpin`, or a worker's `Done` — decoded onto the bus by
  [[map/exarch/shell-eval|shell-eval]]'s `decode_surface` and drawn by one
  generic interpreter ([[map/exarch/cards|cards]]).
- `Inbox` is the typed inbound twin — a per-session queue of `InboxMsg`s, each
  carrying its source and drain boundary. User steering drains mid-turn at a
  tool boundary (`drain_tool`); a scheduled wakeup or a settled async agent
  drains at the turn boundary as its own marked `Turn` (`drain_turn`)
  ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]],
  [[decisions/260617_scheduled-wakeups|scheduled-wakeups]],
  [[decisions/260617_async-agent-tool|async-agent-tool]]).
- a `FleetBus` owns the event channel and the inbox; `pump` borrows it, runs
  the worker on a scoped thread, drains events into the sink, and reports a
  worker panic as a final `Kind::Error`. Completion is the per-turn `done` flag,
  latched by `drain_pass` so a turn ends even while a background producer keeps
  the channel non-empty — never the channel's state
  ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]).
- the channel itself is bounded and coalescing, not a bare `mpsc` pair
  (`BusSender`/`BusReceiver`, same `send`/`try_recv`/`recv_timeout` shape):
  pushing `Token`/`Thinking` (concatenate) or `Phase` (replace) merges into the
  queue's tail entry when it is the same class and the same agent id; every
  other `Kind` — lifecycle, tool frames, cards, errors — is reserved and always
  enqueued on its own, so a producer flood can only ever grow one coalescing
  run, never bury or reorder a lifecycle event. A merged `Token`/`Thinking`
  run's text is capped (`MERGE_TEXT_CAP`, 256 KiB); past it the front elides
  and one `Kind::SystemNote` overflow marker rides the next drain, naming the
  class and the elided count. `/resources` reads `BusReceiver::depth`/`bytes`
  for its `bus.depth`/`bus.bytes` rows.

The TUI mints one **session-lived** bus, so a detached async agent clones its
sender and streams a live tab through the same id-routed draw path a sync child
uses ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]);
headless and test sinks build a **per-turn** bus that closes when the worker
finishes, keeping async children muted to their own log.

`agent/event.rs` is the canonical per-session record. `AgentLog` owns two things:

- the in-memory event mirror — renders the next provider request, drives
  the protocol state machine (`is_ready` gates a fresh prompt and `quiesce` winds
  any in-flight turn back to it, so a turn never strands a prompt mid-protocol;
  [[invariants/turn-ends-ready|turn-ends-ready]]);
- a pretty-printed `events.json`, appended as each event lands — the post-mortem
  "model view". Oversize tool-result sections are elided head+tail at the
  [[map/exarch/agent|digest]] caps before they ever enter the log.

The TUI writes a sibling `user.log` from the same stream — the "user view" —
flushed as each block lands so it survives an abnormal exit. Both files live
under the durable per-run log directory (`bootstrap::log_run_dir`,
`$XDG_STATE_HOME/exarch/<project>/<run>/sessions/<id>/`). Every touch of that
file lives in one place: `tui/viewport.rs` keeps both the tee writer
(`open_log`) and the `/export` copy (`export_log`) beside each other, the
single `user.log` I/O door, so the `/export` handler (`tui/commands.rs`,
`resolve_export_path`) resolves and guards the destination but never reaches
the filesystem itself.

Two `Sink` implementations:

 `tui.rs` (+ `tui/{app,banner,block,commands,fidelity,gesture,group,highlight,line,matrix,md,model_picker,palette,picker,prompt,rail,render,select,status,surface,tabs,terminal,tui_loop,viewport}.rs`) — the full-screen
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
   the base; the human's submitted prompt is opened by a full-width
   `PROMPT_INK` rule fence (`line::prompt_fence`) and neutral prompt ink, found
   at a glance by boundary and tone rather than by reverse video, which stays
   reserved for an active selection.
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
   break commits the real `Block::markdown`. A reasoning run streams the same
   way — a provisional thinking seat ahead of the answer row — and commits as
   one `∴` thinking block per run (`Viewport::commit_thinking`), later deltas
   ticking that block's magnitude in place rather than opening a new one.
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
 arithmetic: `Viewport::render_window` computes `offset` (first visible row,
 topping at `total - height`) over a memoised whole-buffer flatten, and
 reports scroll position as a fixed-position magnitude on the rule line
 (`RenderWindow::scroll_pct`, rendered `⇣ 72%` / `⇣ bot`) rather than an
 animated right-margin scrollbar.

 The `rule_line` carries a value-ramp `ctx%` bar and an elapsed-wait bar
 (elapsed wall-time on the live phase, resetting per round-trip); the live
 phase lives on `Viewport`, not `App`.
 Sub-agent sessions get matrix rows/tabs that linger for 90 seconds
 (`LINGER`, `tui.rs`) after `Died`, each keeping its own scroll position; dead
 rows dim and keep their final step cells without a countdown. The conversing
 trunk is label-only in the matrix — no step cells, token readout, or size bar
 — so those columns describe workers only. An async agent on the
 session-lived bus streams its tab the same way, and `/clear` retires every
 live sub-tab through the same linger window
 ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).
 Once `LINGER` elapses, `Tabs::tick` evicts the dead view into a `Tombstone`
 (`Viewport::evict_to_tombstone`) — exactly agent id, final status, and log
 path, everything else (blocks, the flatten, streaming buffers, pins)
 dropped; no reload-from-`user.log` machinery is built. Every live viewport
 also caps its own retained window — `VIEWPORT_MAX_BLOCKS` blocks and
 `VIEWPORT_MAX_ROWS` rendered rows, oldest evicted first — since older
 blocks are already durable in `user.log`/`events.json`.
 `/clear` also cancels the in-flight model turn: `route_submit` raises
 `cancel::raise_interrupt` and cascades `agents.cancel_descendants(root)` *before* blanking
 the viewport, so the streaming `select!` in `provider::complete` unwinds within
 one `wait_for_cancel` poll (~50 ms) rather than running to its natural end.
 Straggler tokens the worker already emitted into the bus before the
 cancel noticed are dropped by `App`'s `root_clear_drain` guard, which arms in
 `App::clear` and disarms at the next `UserPromptEcho`.
 The TUI owns the REPL loop and the raw-mode / bracketed-paste /
 alt-screen / mouse-capture guard. Every tab shares one submit path: a typed
 line goes to the *focused* agent (the prompt chrome follows the focused tab,
 not the trunk), and a prompt submitted while a turn runs
 is posted to the `Inbox`; the dispatch loop drains non-slash steering at
 the next safe tool boundary, and the rest lands at the turn
 boundary — a coalesced human run, or a wakeup / settled agent as its own
 marked turn. A committed human turn echoes on the `RailShape::Prompt` band;
 a wakeup stays dim, ambient chrome with no rail glyph (`RailShape::Plain`).
 Slash-prefixed prompts
 stay on the REPL command path (`tui/commands.rs`, parsed uniformly on every
 tab). View commands (`/help`, `/legend`, `/copy`,
 `/export`, `/model`, `/login`, `/resources`) run on the UI thread; session commands
 (`/clear`, `/compact`, `/branch`, `/quit`) enter the focused
 agent's inbox as `Command` turns and run in `ReplControl`. `/branch`
 forks a *conversing* tab from the focused context — a peer conversation
 under [[decisions/260705_branch-minimal|branch-minimal]] — and `/close`,
 the one command admitted off the trunk, kills the focused branch and its
 subtree. The idle wait
 selects over input, inbox, and the session bus
 ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]],
 [[decisions/260617_scheduled-wakeups|scheduled-wakeups]]).
- `/model` and `/login` share `picker::overlay_frame`: one centred double-line
  bezel, shadow, palette, padding, title, and hint frame around distinct
  bodies. The login body drives browser or device OAuth on a background thread,
  receives typed `LoginPhase`s over a channel, and carries the device expiry
  label from the flow rather than reconstructing it in the view. Closing the
  overlay sets its relaxed cancellation flag; browser accept polls it directly,
  while device polling checks it before each bounded request.
- `headless.rs` — one-shot pipe: assistant tokens to stdout (or, under
  `--output-format json`, one result object built from the root's `reply`),
  every other event condensed to one line on stderr, exit after one seed turn.
  Takes the default `Sink::drive` and a per-turn bus, so its async children stay
  muted. It is a display only — the durable `transcript.jsonl` / `events.json`
  are written by each session's own `agent/transcript.rs` / `agent/event.rs`
  seams, in headless exactly as in the TUI.

`agent/cancel.rs` is the per-agent turn cancellation layered on ral's interrupt
handling. Every agent holds one **sticky** `Token` (an `Arc<AtomicU8>`) for its
whole drive; the drive loop `reset`s it at each genuine turn boundary. Esc /
Ctrl-C interrupt the *focused tab's* current turn — never a cascade, never a
subtree kill ([[decisions/260705_cancel-per-tab|cancel-per-tab]]): on the trunk
they route through `raise_interrupt`, which cancels the trunk's published token
and asks ral to cancel the current turn's foreground scope; on any other
focused tab, `agents.interrupt(id)` unwinds that agent's turn and eval root
through the registry. Only the trunk `publish`es its token's flag into the
lock-free process-global slot for the OS signal handler (a handler must not
lock), so the provider's mid-stream cancel race observes the same cancellation.
The TUI key table keeps UI control separate from cancellation: idle Ctrl-C/Ctrl-D
quit, overlays close, and only active-turn Ctrl-C/Esc drive ral's
non-escalating foreground cancel. A single press stops the turn loop /
in-flight HTTP future and unwinds the in-flight eval at its next poll point;
because the path never escalates the signal count, repeated presses cannot
reach ral's third-signal `_exit`
([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).
On Windows the same contract rides `SetConsoleCtrlHandler`: `install` registers
exarch's routine after ral's, so it runs first in the last-registered-first
chain, handles Ctrl-C/Ctrl-Break itself — `raise` plus ral's non-escalating
`relay_interrupt`, which cancels the foreground scope and fans a Ctrl-Break to
every live, non-detached pipeline group — and reports them handled, so ral's
escalating disposition never ticks for a turn-cancel; window-close / logoff /
shutdown pass through unhandled to that disposition, the analogue of
SIGTERM/SIGHUP staying on the escalating path. Raw mode disables
`ENABLE_PROCESSED_INPUT`, so Ctrl-C reaches the TUI as an ordinary key event
and `deliver_interrupt` calls the relay in-process — never a
`GenerateConsoleCtrlEvent` re-injection, which would broadcast to the console
group and tick ral's escalation counter.
A genuine external signal still routes through ral's one cause-carrying
delivery path ([[decisions/260706_signals-are-causes|signals-are-causes]]).
Exarch session shells are rebuilt only through `bootstrap::boot_shell`, which
discards stale ral interrupts before library loading and returns with the
cancel chain installed over ral's handlers. `/clear` therefore works after Esc
and SIGINT after `/clear` still raises cancel. `prompt/host.rs` snapshots the machine (OS, date, cwd,
user, git state) once at startup for the [[map/exarch/policy|system prompt]].
        - `tui.rs` — thin façade (~60 lines): module declarations and re-exports
        - `tui/app.rs` — the `App` orchestrator: event routing, the `root_clear_drain` guard, per-kind push methods
        - `tui/tui_loop.rs` — REPL/ui loop: `run`, `Tui`, `CommandCtx`, `ReplControl`, `ui_loop`, `KeyMode`, `KeyAction`, `key_action`, `ctrl_key`
        - `tui/terminal.rs` — terminal lifetime: `TerminalGuard`, raw mode, alt screen, panic hook, stderr redirect, editor hatch, `compose_in_editor`
        - `tui/tabs.rs` — session/view lifecycle: `Tabs`, viewports, dispatch order, tabs, titles, dying linger, parent chain, focus management, `tick`'s tombstone eviction past `LINGER`
        - `tui/viewport.rs` — per-session scrollback: `Viewport`, block push/flatten/render, the `VIEWPORT_MAX_BLOCKS`/`VIEWPORT_MAX_ROWS` window caps (oldest evicted first), `Tombstone`/`TombstoneStatus`
        - `tui/surface.rs` — event coalescing: `SurfaceBuffer`, `PatchBuf`, `ObservationBuf`, absorb/flush operations
        - `tui/prompt.rs` — prompt editor state: `PromptState`, history, draft, editor request, key input
        - `tui/gesture.rs` — mouse/selection: `GestureState`, `Press`, frame geometry, selection, copy toast, hover, scroll
        - `tui/render.rs` — frame layout: `draw`, `FrameGeom`, `paint_selection`, `paint_hover`, `footer_hint`, `emit_tab_title`
        - `tui/banner.rs` — startup metadata: `SessionInfo`, `session_card`, `legend_panel`, ART/EAGLE constants
        - `tui/commands.rs` — slash command registry: `SlashCommand`, `lookup_command`, `route_submit`, handler functions
        - `tui/status.rs` — status line: `StatusReadout`, `rule_line`, `ctx_ramp`, `wait_bar`, `wait_step`
        - `tui/matrix.rs` — agent matrix and tab bar: `MatrixSort`, `matrix_bar`, `tab_bar`, justified row projection, `step_cells`
        - `tui/palette.rs` — the TUI colour constants (`CODE_BG`, `SLATE`, `PROMPT_INK`, the agent hues)
        - `tui/model_picker.rs` — model switching: `pick_model`, `drive_picker`, `apply_model_switch`
