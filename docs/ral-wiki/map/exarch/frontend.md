---
generated_at_commit: f448310e
generated_at_date: 2026-10-19
covers_paths: [exarch/src/bus.rs, exarch/src/bus/post.rs, exarch/src/bus/inbox.rs, exarch/src/bus/signal.rs, exarch/src/bus/channel.rs, exarch/src/bus/emitter.rs, exarch/src/bus/sink.rs, exarch/src/record.rs, exarch/src/record/, exarch/src/record/session.rs, exarch/src/tui.rs, exarch/src/tui/, exarch/src/headless.rs, exarch/src/cancel.rs, exarch/src/boot.rs, exarch/src/agent/seat.rs, exarch/src/signals.rs, exarch/src/prompt/host.rs]
---

# Map: exarch / frontend

The transcript calls shell workers **errands**: `errand #N started · description`
and `errand #N finished (exit S) · description`, with failure reasons preserved.
Birth and completion share the registry ID; descriptions come from existing
call-site labels or explicit `watch`/`service` descriptions. Saved completion
records without an ID remain readable and omit it. `spawn` and `defer` retain
their existing signatures.


The agent core and the user interface meet at **one outbound event stream and
one inbound inbox**, mapped by `bus.rs`'s module doc across its submodules:

- workers stamp a `Record` — `Protocol`/`Display`/`Forensic` (below) — through
  `record::Emitter::emit` (`record/seam.rs`), which appends to `record.jsonl`
  and publishes the witnessed `Recorded<Record>` onto the fleet channel in one
  lock; the channel's other passenger is a live-only `Transient` — a streamed
  token or reasoning delta, a state transition, a worker's birth/death, the
  seam's own fault — with no sequence number and no durable form, published
  through `Emitter::transient`. Together these are the two `Signal` variants
  (`bus/signal.rs`) a `Sink` (`bus/sink.rs`) consumes. `AgentState` is the five
  states an agent is ever in (`Ready`/`AwaitingModel`/`Evaluating`/`Evicting`/
  `WaitingOnAgents` — a total state named on the status rule, never recorded:
  the model never sees it), riding as `Transient::State`. A decoded surface
  class — a `Card` render document a kit raises through the `exarch-surface` builtin,
  a structural observation, a housekeeping notice, a `Pin`/`Unpin`, or a
  worker's `done` — is decoded by [[map/exarch/shell-eval|shell-eval]]'s
  `decode_surface` into its own closed `Surface` vocabulary first, then
  recorded as a `Display` commit by the commit producer (`record/commit.rs`)
  and drawn by one generic interpreter ([[map/exarch/cards|cards]]).
- `Inbox` (`bus/inbox.rs`) is the typed inbound twin — a per-session queue of
  `Post`s (`bus/post.rs`). A pull yields a `Next`: an `Item` for the model, a
  `Read`, or a `Rewrite`. An **exchange** on this page is a human round — what
  the fleet's clock counts, what a boundary ends and a key interrupts — never
  a unit of the context, which has only turns, each with a role
  ([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]).
  A typed line is one of three classes, decided once at submit on the UI
  thread (`tui/commands.rs::route_submit`): an **immediate** view command
  (`/help`, `/copy`, `/model`, …) acts there and is never posted; a **read**
  (`Post::Read` — `/new`, `/branch`, `/context`, `/resources`) drains at the *next*
  boundary of any kind, tool or exchange, in the order typed; a **rewrite**
  (`Post::Rewrite` — `/clear`, `/close`, `/evict`, `/rewind`, `/quit`) drains only at
  the exchange boundary and holds everything typed after it, because there the
  order typed is part of what was said. Everything else — prompt text,
  including a slash-shaped line that names no command, wakeups, settled
  agents, surfaces, peer messages, nudges — is a delivery and drains at the
  next boundary in order. So the variant alone decides the drain point:
  `drain_mid_exchange` (from `agent::deliberate`, after each tool batch)
  yields the reads and deliveries in the order typed and stops at a rewrite;
  `next_or_idle` pops anything. Within one tool boundary each arrival is taken
  at its place: a read runs against the context as it then stands, a delivery
  lands as a steering line of its own — so a `/branch` typed beside a prompt
  forks with it or without it exactly as typed. The attend thread runs the commands
  itself (`Avatar::read`, `Avatar::rewrite` in `agent/command.rs`); there is
  no frontend hook ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]],
  [[decisions/260617_scheduled-wakeups|scheduled-wakeups]],
  [[decisions/260617_async-agent-tool|async-agent-tool]]).
- a `FleetBus` (`bus/emitter.rs`) owns the channel, and mints each root's
  emitter over that root's own mailbox; `pump`
  (`bus/sink.rs`) borrows it, runs
  the worker on a scoped thread, drains signals into the sink, and reports a
  worker panic as a recorded `Forensic::Error`, prefixed so a sink can tell it
  from a clean completion (`bus::WORKER_PANIC_PREFIX`). Completion is the
  per-exchange `done` flag, latched by `drain_signals` (`bus/sink.rs`), so an
  exchange ends even while a background producer keeps the channel non-empty —
  never the channel's state; the TUI's own `drain_signals` (`tui/tui_loop.rs`)
  reads the fleet's emptiness before each pass instead, since its run ends
  with its last root
  ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]).
- the channel itself (`bus/channel.rs`) is coalescing but unbounded in depth, not a bare `mpsc` pair
  (`BusSender`/`BusReceiver`, same `send`/`try_recv`/`recv_timeout` shape):
  pushing `Token`/`Thinking` (concatenate) or `State` (replace) merges into the
  queue's tail entry when it is the same class and the same agent id; every
  other `Signal` — every `Fact`, and every other `Transient` — is reserved and
  always enqueued on its own, so a producer flood can only ever grow one
  coalescing run, never bury or reorder a fact. A merged `Token`/`Thinking`
  run's text is capped (`MERGE_TEXT_CAP`, 256 KiB); past it the front elides
  and one `Transient::Fault` overflow marker rides the next drain, naming the
  class and the elided count. `/resources` reads `BusReceiver::depth`/`bytes`
  for its `bus.depth`/`bus.bytes` rows.
- what the channel carries is `Signal` (`bus/signal.rs`): a seam-witnessed fact
  (`Signal::Fact`, an `AgentId` beside the log `Segment` it was appended to and
  its `Recorded<Record>`), an agent's birth (`Signal::Born`, sent by minting
  the agent's emitter, `bus::Emitter::child`, so it precedes everything that
  emitter carries), or an unrecorded delta (`Signal::Transient`, the `AgentId`
  beside its `Transient`) — one channel, so the TUI's single per-fleet
  dispatch loop routes all three by `AgentId` without restructuring.

`record.rs` + `record/` is the one-seam-one-log module tree
([[internals/session-record|session-record]]): the sealed
`Record { Protocol, Display, Forensic }` vocabulary and the disjoint
`Transient`; `record/seam.rs` (`Emitter::emit` above, the only publisher —
its one lock is the log's own mutex, so channel order is log order); `record/log.rs` (the `record.jsonl` syscall site, with the attachable
`FleetSink` inside the writer's mutex — a record appended with no sink, or
onto a dead channel, is published nowhere — and `Log::since`, a mirror's
catch-up, which fixes the current segment, its extent and a read handle under
that same mutex and streams the records past a `record::Cursor` outside it, so
whatever lands after the extent reaches the channel instead); `record/replay.rs` (the generic
`fold == memo` driver and `Refusal`, with `Log::read` streaming one entry at a
time); `record/commit.rs` (the worker-side
commit producer: one `Chopper` per lane of the model's stream, cutting it into
records at the last newline it holds); `record/model.rs` and its
`model/` children (the model fold, `Protocol` alone, with streaming
resume/admission, and the owned `Vec<ChatMessage>` the provider-facing
projection is built as, on call and memoised nowhere — see
[[internals/session-record#The provider-facing context is a pure function of the structure|the context-as-projection section]]
and [[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]]);
`record/view.rs` (the view fold into `Blocks`; block construction is private).
A frontend has the `bus::Sink` shape (`fact`/`transient`): it is itself a
fold over the one log, owning its own `Blocks` memo,
stepping it per witnessed record (`Sink::fact(id, rec)`) and acting on the
`Delta` that step reports — `Opened`, `Grew`, `Patched`, `Quiet`
([[decisions/260909_the-fold-reports-the-printer-mirrors|the-fold-reports-the-printer-mirrors]]).
It is handed the record only to step that memo, and renders from
`record::BlockKind` off it. `Headless` implements `Sink` for the pipe; the
TUI's `App` has the same `fact`/`transient` pair without the trait, its
`ui_loop` draining on the render cadence rather than `Sink::drive`'s blocking
one, and owns one `Scrollback` per session, each a mirror with a memo of its own
and a `record::Cursor` into its log — the `Segment` it holds, and the `Seq`
and closing byte of the last record folded. The channel is only the mirror's fast path:
`Scrollback::fact` steps a fact that is next, and reads the log
(`Scrollback::catch_up`, over `record::Emitter::since`) whenever the channel
cannot vouch for one — at birth, past a gap, on a newer segment — while a
straggler from an older segment is dropped and a record heard twice folds once.
So birth, resume, a gap and `/clear` are one construction, and no frontend
depends on the order a fact and a birth reach it. A `Sink` (headless, synod)
narrates what it hears and never catches up.

The layering is a DAG ([[invariants/exarch-is-a-dag|exarch-is-a-dag]]): `record`
publishes through its `Publish` trait and knows no `bus`, and `bus` knows no
`agent`, so the seam above is the only way a worker's fact reaches a frontend.

The TUI mints one **session-lived** bus, so an async agent clones its
sender and streams a live tab through the same id-routed draw path a sync child
uses ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]);
the one-shot/headless conversational drivers use a **per-exchange** bus with
muted children, while `converse_settled` uses `per_exchange_live` so fleet work
stays visible until quiescence.

`record/session.rs` is the canonical per-session record. `AgentLog` owns two things:

- the model fold's `Context` (`record/model.rs`) — its only session state,
  and the one every query about the context is asked of (`AgentLog::context`):
  it drives the protocol state machine (`is_ready` gates a fresh prompt and
  `quiesce` winds any in-flight exchange back to it, so an exchange never
  strands a prompt mid-protocol; [[invariants/turn-ends-ready|exchange-ends-ready]])
  and answers `render_messages()` — the provider-facing `Vec<ChatMessage>`,
  built off the structure on call with no memo behind it — which
  `provider/wire.rs` alone turns into an owned request, once per HTTP attempt
  ([[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]]);
- the record seam (`record::Emitter`) onto `sessions/<n>/record.jsonl` — the
  one durable log every fact this session authors crosses, whose protocol
  fold is the model's context, with display commits and forensic breadcrumbs
  beside it ([[decisions/260814_one-seam-one-log|one-seam-one-log]]).
  Oversize tool-result sections are elided head+tail at the [[map/exarch/agent|digest]]
  caps before they ever enter the log. A directory with no `record.jsonl`
  refuses to resume, with a named error. `/clear` rotates the record segment
  behind the same seam, so existing emitters and the attached bus continue
  into the fresh file, and bumps the log's `Segment`, which every fact
  published from then on carries.

The TUI renders a sibling `user.log` — the "user view" — as a stream the
scrollback is a window over: a block is written once, when the mirror's head
trim drops it (`Scrollback::trim`), into a retired prefix that only ever grows,
and the blocks still resident are written past that prefix provisionally at
flush points (session end, `/export`), which the next retirement rewinds over
rather than repeating
([[decisions/260816_the-window-is-not-the-transcript|the-window-is-not-the-transcript]]).
So the file keeps the whole session however long it runs, while the window
keeps only what fits; crash durability still lives in `record.jsonl`, which is
flushed per record. Both files live under the durable per-run log directory
(`App::log_run_dir`, `app.rs`,
`$XDG_STATE_HOME/exarch/<project>/<run>/sessions/<id>/`). Every touch of that
file lives in one place: `tui/scrollback.rs` keeps both the writer (`Log`) and
the `/export` copy (`export_log`) beside each other, the single `user.log` I/O
door, so the `/export` handler (`tui/commands.rs`, `resolve_export_path`)
resolves and guards the destination but never reaches the filesystem itself.

On resume, a resumed root's `Born` carries its `Resumed` summary, whose
`bytes` is the log's extent before this run wrote to it. Its tab mirrors that
history by `Scrollback::recall` — the birth catch-up, stopped at that byte,
with `user.log` writes withheld and the mirror marked as already in that file,
written by the run that recorded it — then draws the "resumed" note and
catches up on whatever this run has recorded since, so the resumed transcript
continues rather than repeats, and the note sits at the true boundary however
late the tab is born. Every reopened root does so for its own tab. The run's
usage is read off no view: `App::total_usage` is the session bus's own
`UsageMeter`, which tees every usage record at the seam once, plus what the
resumed histories had spent. For the running session
the scrollback is fed one `Signal` at a time: `tui_loop::dispatch` routes a
`Signal::Fact` to `App::fact` (the scrollback's cursor, above), a
`Signal::Born` to `App::born` (a tab, and its catch-up), and a
`Signal::Transient` to `App::transient` (drawn directly, with no fold behind
it).

Two presentation surfaces, both folding the one `Signal` vocabulary through
`fact`/`transient`:

 `tui.rs` (+ `tui/{app,banner,block,commands,dial,diff,fidelity,gauge,gesture,group,highlight,line,login,matrix,md,model_picker,overlay,palette,picker,prompt,providers,rail,render,resources,rewind,row,scrollback,select,status,tabs,terminal,text,tui_loop}.rs`) — the full-screen
 TUI. It owns the alternate screen and its own scrollback: each session is a
 `Vec<Block>` (`tui/block.rs`) mirroring the view fold one incident at a time,
 and the whole frame is redrawn each tick from each block's own memoised visual
 rows. A burst of `ral` work is the collapsible object — its tip call's intent
 shows shut, every call with its ral source when a click opens it — highlighted, or solid red when the run failed; the wheel scrolls, click-drag selects and copies the
 rail-stripped text via OSC-52, and Shift-drag falls through to the terminal's
 own selection. `tui/md.rs` is the streaming markdown renderer — a ral code
 block in the model's prose (tagged `ral`, or untagged, which is what the
 indented blocks of `data/ral.md` teach) goes to `tui/highlight.rs`, the same
 lexer-backed colouring the tool-call panels use, so the language reads the
 same wherever it appears, and only a foreign language falls to syntect and
 the `two-face` set. `$…$` and `$$…$$` are typeset rather than quoted: the
 LaTeX goes to `tui-math`, which lays it out on a character grid — fractions
 stacked over a vinculum, big operators carrying their limits. Inline `$…$`
 joins the sentence only as a one-row grid, which is notation; a taller one
 would break the paragraph, so its source stands in. Display `$$…$$` is always
 a block of its own, inset from the prose, even at one row. A grid too wide for the wrap budget, or a formula
 the parser refuses, falls back to the LaTeX the model wrote, inked as the
 literal it is. `tui/group.rs` renders the `▸`
 part of a group — one burst of `ral` work as a single dialable object, its
 body at one of three `Detail` rungs (`Tally`, `Summary`, `Full`);
 `tui/scrollback.rs` the per-session mirror of the view fold, scroll position,
 and `user.log` writer; `tui/rail.rs` the data-encoding marginal rail, and
 `tui/row.rs` the `Row { gutter, content }` that *represents* the margin so
 copy and selection can never reach it.

 Nothing is laid out before its width is known. `Scrollback::render_window`
 caps the pane at `READ_W` — the reading measure, applied in that one place —
 `Block::railed` takes the content columns off it, and every builder below is
 handed that width: chrome renders at it, a card's marks are dispatched at it
 by `render_mark`, and a call's effects and its ral source are laid out at
 what remains after their indent. There is no fixed budget to lay out against
 and then wrap a second time; each row is folded exactly once.

 The
 transcript is laid out as a graphic on two orthogonal planes
 ([[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]],
 Phases 0–7 landed):

 - **two voices, encoded as foreground and background.** *The transcript
   collapses to two parties — the human and the agent field — read off
   orthogonal channels so neither competes with the other.* The agent owns the
   chromatic foreground (the rail hues); the background plane carries one
   distinction only — machine text. A run of script or observation output is
   washed into a recessed panel (`group::wash_inset`): a *left-inset*
   rectangle whose left edge aligns with the content — so it nests under its
   intent, and script and output share one margin to read as a single region —
   and whose right edge still runs to the margin, so the wash reads as a stratum
   rather than a content-hugging swatch. The two are told apart by depth alone:
   the script a call ran wears `CODE_BG`, what it read, ran and searched wears
   the darker `EFFECT_BG`, one plane below. Every intent in a run opens flush in
   content space — the rail's margin is its only indent, so the head call's
   intent sits in the same column as its siblings — and both panels step in
   by one `BODY_INDENT` of two columns. Model prose sits unwashed at
   the base; the human's submitted prompt is opened by a full-width
   `PROMPT_INK` rule fence (`line::prompt_fence`) and neutral prompt ink, found
   at a glance by boundary and tone rather than by reverse video, which stays
   reserved for an active selection.
 - **notifications share one text layout.** `Chrome::render` wraps notification
   text at the block's content width; cards and structured readouts own their
   layout. Agent completions are `Chrome::Subagent`, sharing text flow and gap
   placement with worker completions. Only the first content row wears the rail.
   A completion records its `record::Settlement` (finished, cancelled, or failed
   with a reason), so no consumer reads cancellation out of a reason string.
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
   (`Scrollback::agent`, threaded into `Block::fill` at render time), so the
   whole rail glows one hue, read on a tab-switch as "whose transcript is
   this". The human's prompt fence is the lone exception — a `❖` in neutral
   `PROMPT_INK` so it never reads as just another agent's mark.
 - **the in-flight reply as an open line.** The worker cuts a `Display::Answer`
   at every newline it completes, and the view fold grows one block from the
   run of records that meet (`Blocks::push`) — so the assistant's prose lands
   in scrollback as it is spoken, a line at a time, in one block per run.
   Where a cut falls therefore carries no meaning, which is what frees the
   producer from ever having to find a safe place to break. What no record
   covers is exactly the text past the last newline, and `Scrollback::live_tail`
   renders that open line *inside the block that will absorb it*: the trailing
   block's own source plus the open line plus the newline it is about to gain,
   in that block's own `Fidelity`, drawn through the one path a committed
   block draws by — `Block::railed` takes the open line as a parameter and the
   result is never memoised — so the record that completes the line changes
   the text and not the picture, and the markdown context the line sits in (an
   open fence, a list) is the block's own. The line joins the tail block
   exactly when its record would grow it — `Block::takes_thinking` asks the
   group's own admission rule — so a thinking line after work is drawn in the
   group's `∴` part above that work, where its record lands, and never in a
   provisional block at the foot that the commit would delete. At most one
   lane is ever open: prose ends the thinking run on the mirror's side
   exactly as it does on the worker's, so `Transient::Token`
   clears the thinking lane's open line as `Stream::push` flushes it, and
   `Transient::Boundary` clears whatever is left when no record will cover it.
   Thinking therefore streams as thinking — dimmed through
   `md::render_thinking`, on the `∴` rail — rather than as a magnitude, and
   its tail records where the prose after it resumes, never at the step's end,
   which is exactly where a `∴` must not land. Its grain header weighs the
   deliberation against the prose it became, which the record cannot carry (it
   precedes it) and the view therefore measures: `Scrollback::grain` stamps the
   group above a prose block with that block's own mass as it opens and grows.
   A turn reads *deliberation, then work* because that is how the mirror
   *builds* it, not how a projection reorders it: `Thinking`, a `ToolCall`
   with a stated intent, an observation-origin card and a turn rule all join
   the `Block::Group` standing at the tail, which renders its `∴` part above
   its `▸` part; every other kind is a barrier that pushes its own block and
   ends the group. Blocks, their record ids and `user.log` are all in arrival
   order, and so is the group — only its two parts are read in a fixed order.
   Each part carries its own `Dial` — the `Detail` rung it is read at and the
   floor a cycle wraps to — so one group answers two dials and
   `Scrollback::block_at` reports which part a row belongs to; the deliberation's
   two rungs are its grain header or the whole thing. The `∴` part also
   answers to one standing rung, `/thinking`'s datum: `Tabs::thinking` holds it
   because it outlives any one view, and `Scrollback::set_thinking_level` moves
   every group on screen while every group still to arrive is born there. That
   rung starts at `Detail::Summary`, so a session opens with deliberation read
   as its grain and bulk — including the live one, whose header is a magnitude
   growing in place rather than text streaming past. `Block::dial` is the one
   answer to which part of a block has a dial: whether a part is dialable and
   what a click does both read it, as does the rung a one-part block renders at.
 - **a surfaced general card as a bounded object.** A diff-less
   `CardOrigin::Surfaced` card — the model's deliberate "look at this" —
   renders through `line::render_card_framed` as a box indented `CARD_INDENT`
   columns, its heading lifted into the top rule, no marginal rail glyph (the
   frame is its mark) — though it still wears the blank margin every row wears,
   so its left edge aligns with the rest of the transcript. A file mutation — a run of changes or a kit's diff card — wears
   the patch-shape change-bar `▎`; an observation card folds into its ral group.
   A cancelled turn is `Chrome::Cancelled`: it wears the error rail `╳`
   while remaining distinct from `Chrome::Error`, so
   `Scrollback::last_is_error` — read off the fold, the failure being a fact of
   the record — and the matrix failure cell report actual failures only.

 The frame's terminal writes are bracketed in a synchronized update
 (`BeginSynchronizedUpdate` / `EndSynchronizedUpdate`) so the emulator swaps
 the whole diff atomically — without it a tail-following redraw tears while a
 full page streams tool calls. The same steadiness is held in the scroll
 arithmetic: `Scrollback::render_window` computes `offset` (first visible row,
 topping at `total - height`) over one walk of the mirror's memoised rows, and
 reports scroll position as a fixed-position magnitude on the rule line
 (`RenderWindow::scroll_pct`, rendered `⇣ 72%` / `⇣ end`) rather than an
 animated right-margin scrollbar.

 The `rule_line` carries a value-ramp `ctx%` bar, the agent's state in a
 fixed-width slot, and an elapsed-wait bar reading the time *in that state* —
 anchored to the transition, never reset by an arriving event, so a
 streamed-character count that has stopped growing under a rising `Ns` is a
 stalled stream and reads as one. `AwaitingModel` is named by that count
 alone rather than by its label, the count being the datum the elapsed bar
 gives meaning to. Two adjacent magnitudes need telling apart, so the clock
 holds three columns and its unit (`  12s`, then whole minutes past `999s`),
 the count carries its own (`1.2k chars`) and the wait bar's purple, and the
 rule's ` · ` separates them — separation and units, never a hue that means
 one thing this frame and another the next. Every field on the rule takes that
 same separator, the right-aligned usage block included, since the elastic
 space before it collapses on a narrow terminal; the state slot is padded to
 `STATE_SLOT`, the longest label's width, so the separator after it stands in one column whatever the
 state. `Ready` waits on nothing, so it draws the bar's empty track and no
 clock: the track is the scale the filled cells are read against and stays put
 between turns, while the readout goes blank, nothing being timed. The width is
 the same either way, so no field shifts. The `StateSpan` (state, entry instant,
 characters streamed since) lives in the tab's `Status` (`tui/tabs.rs`, beside
 its pins), not on `App`, so each tab times its own.
 A tab is the birth facts off its `Born` notice (name, parent, log path), its
 `Status` (state span and pins), and its `Life` — `Live` or `Lingering` over
 a scrollback, which holds the agent's own record seam, both to catch up from
 and to record a command the frontend owns (`/model`) into that
 tab's log, then `Gone`: one `Tab` per agent the stream has announced,
 in a single `Vec` in id order. There is no root tab: every root is a tab born
 by its own `Signal::Born`, and `Tabs` is non-empty by construction:
 `tui_loop::run` takes the run's first signal, which is the first root's birth
 because its own spawn loop minted that root's emitter before any thread
 existed, and `App::new` builds `Tabs::new` around it, giving that first root
 the same card and catch-up (`App::welcome`) as any later one. A stale focus resolves to the first *attended* tab (in the bar,
 not lingering), else the first in the bar, else the first tab, the same
 predicate `parent_focus` climbs by. The frontend holds the
 `Fleet`, to resolve a tab's name and to reach the live roots on `/quit`, and
 no `Arc<Agent>`: reach is `Tabs::agent(id)`, an upgrade held for one handler
 and never stored, so the avatar alone decides how long an agent lives. Liveness and death are
 different questions asked of different things — a failed upgrade says the
 agent is gone *now*, while `Transient::Died` sits at a *position* in the bus
 queue, and only that position can say which buffered events preceded it, so
 `Died` (and `/clear`'s `retire_subtree`, the frontend declaring its
 descendants' death ahead of the fact) remains the death cue, and `App` drops
 everything a lingering tab is sent after it. The birth facts ride the notice rather than being read back off the
 agent, so a child that settles before the frontend drains its `Born` still
 gets a properly labelled tree row. `App`'s matrix navigation is modal, and
 its whole retained state is `Matrix` — the agent identity the cursor names,
 or nothing while the strip is a status display. `Tab` enters and leaves,
 `↑`/`↓` and Shift-Tab move the cursor, `Enter` attaches to its row *and*
 leaves navigation, so attach-and-type is one gesture, and `Esc` leaves the
 surface without cancelling the focused exchange; a cursor whose agent has
 gone reads as the attached tab. The drawn window is a pure function of that
 cursor — it always holds the cursor's own row and spends one line on each
 side it hides — and connectors are derived from the whole spawn forest before
 the window clips it. While navigation owns the keyboard the strip is always
 drawn, taking a row from the transcript if it must; otherwise it fits in
 whatever the transcript's floor leaves over. Demotion — a tab idle and
 parked past `DEMOTE_IDLE` (`tui.rs`, beside `LINGER`) becoming a compact
 slate row in place, keeping its position in the spawn tree, the focused tab
 alone exempt — is read off the
 agent's own exchange clock, computed per row per frame and never stored.
 Sub-agent sessions get matrix rows/tabs that linger for 90 seconds
 (`LINGER`, `tui.rs`) after `Died`, each keeping its own scroll position; dead
 rows dim and keep their final turn cells without a countdown. Every row,
 roots included, carries turn cells, a token readout and a size bar. An async
 agent on the session-lived bus streams its tab the same way, and `/clear`
 retires the cleared agent's descendant tabs through the same linger window
 ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).
 Once `LINGER` elapses, `Tabs::tick` turns the tab `Life::Gone`: the
 scrollback (blocks and their render memos, streaming buffers) is dropped
 after its blocks are retired to `user.log`, and the `Status` is reset,
 releasing its pins; only the log path stays, which `Tabs::status` and
 `status_mut` no longer answer for; no reload-from-`user.log` machinery is built. Every live
 scrollback is bounded by exactly one window, the view fold's own:
 `BLOCKS_WINDOW` resident blocks (`record/view.rs`), oldest dropped as new ones
 land. After every step `Scrollback::trim` walks the mirror's head against the
 fold's first block — one comparison — and what it drops is retired to
 `user.log` on the way out, so a block is durable by the time it leaves heap.
 Chrome, which no record authors, inherits the expiry of the block it was drawn
 after; the banner, drawn before any block, lives while the fold still holds
 the session's opening block (`Blocks::origin`). Nothing is rebuilt: a record
 moves one block, and the mirror moves that block
 ([[decisions/260909_the-fold-reports-the-printer-mirrors|the-fold-reports-the-printer-mirrors]]).
 `/clear`, `/close` and `/quit` share one helper: interrupt the agent, then queue
 the `Rewrite` behind the interrupt, so the streaming `select!` in
 `provider::complete` unwinds within one `wait_for_cancel` poll (~50 ms)
 rather than running to its natural end. `/clear` does not blank the view at
 the keystroke: the cleared log's first fact arrives under a newer `Segment`,
 `Scrollback::fact` starts the mirror over on it — reset, the session card,
 a catch-up of the new segment — and reports it, and `App` retires `id`'s
 descendant tabs (`Tabs::retire_subtree`). A clear that failed rotates
 nothing, so the view stays; the run's total usage is not reset.
 The TUI owns the REPL loop and the raw-mode / bracketed-paste /
 alt-screen / mouse-capture guard. Every tab shares one submit path: a typed
 line goes to the *focused* agent (the prompt chrome follows the focused tab),
 and a prompt submitted while an exchange runs
 is posted to the `Inbox`; `run_batch` drains non-slash steering at
 the next safe tool boundary, and the rest lands at the exchange
 boundary — a coalesced human run, or a wakeup / settled agent as its own
 marked item. A committed human prompt echoes on the `Chrome::Prompt` band;
 a wakeup stays dim as note chrome (`Chrome::Note`).
 Slash-prefixed prompts
 stay on the REPL command path (`tui/commands.rs`, parsed uniformly on every
 tab). View commands (`/help`, `/legend`, `/copy`,
 `/export`, `/model`, `/login`, `/providers`, `/limits`, `/thinking`, `/focus`) run
 on the UI thread; `/help` answers with a framed card of `(command, gloss)`
 rows, and `/copy` answers where every copy does — the transient corner
 toast of `gesture::Toast`, never a block in the transcript; copies go
 through the `clipboard` crate, native locally and OSC 52 over ssh;
 session commands are parsed at submit into a `Read` (`/new` and `/branch`
 first typed as the one `Command::Hatch(Memory, Option<name>)`, mirroring
 `Read::Hatch`; `/context`, `/resources`) or a `Rewrite` (`/clear`, `/close`, `/evict`,
 `/rewind`, `/quit`) and enter the focused agent's inbox; `/quit` alone posts
 to every live root, and a session command on a tab whose agent has ended
 answers "this tab's agent has ended". Every command acts on, and reports
 into, the focused tab: a tab is a tab, and none is the root. `/rewind` takes
 no argument: it opens
 the `tui/rewind.rs` overlay over the focused tab's turn-opening prompts (the
 fold's `Blocks::prompts`, since `Display::Prompt` carries the `turn` it
 opens — `None` for a steering line, which opens none), the latest
 highlighted. The eight-row list keeps the selection near its centre, shows
 its position in the history, and supports Page Up/Down and Home/End. While
 it is open, `Scrollback::render_cut_window` positions the cut a third down
 the transcript, clamped at either end, and returns its row, from which
 rows are ghosted (`Row::ghost`) in `render::draw`. The viewport is restored
 on close, and the preview is drawn only while the overlay's tab is the
 focused one; ⏎ posts `Rewrite::Rewind(turn)` to the tab the overlay was
 opened on, not to wherever focus has since moved. Both cuts use `Blocks::rewind_point`.
 `/new [name]` and `/branch [name]` open a *root session*: a new tab with an
 engine of its own, named by the human or by a minted `root-{N}`, and parked
 for them rather than seeded with a prompt — blank for `/new`, carrying the
 focused tab's context for `/branch`, with a fresh shell either way. `/close`
 ends the focused tab's session for good (`--resume` will not reopen it) and
 `/quit` ends every root, resumable; the process exits once the last root's
 avatar has dropped (`Fleet::wait_empty`, woken by each worker's
 `Fleet::departed`, not polled). The idle wait
 selects over input, inbox (`bus/inbox.rs`), and the session bus (`bus/channel.rs`)
 ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]],
 [[decisions/260617_scheduled-wakeups|scheduled-wakeups]]).
- The command token completes **as it is typed**, not on request: while the
  first row is a bare `/` and command characters, `commands::command_candidates`
  fuzzy-ranks every name and alias (`ral_core::text::rank`) and `PromptState`
  holds the result in a `prompt_editor::completion::Menu`, floated out of the
  top of the prompt box over the transcript — a transient overlay that reserves
  no row, so the frame never shifts under the reader. A live popup owns ↓/Tab,
  ↑/⇧Tab, Enter and Esc: Enter takes the highlighted command *into the line* and
  closes rather than submitting, since the line under an open popup is not yet
  what the user has chosen, and a second Enter sends it — unless the line
  already *spells* the highlighted command, where accepting would be the
  identity: the popup then closes and declines the key, which falls through to
  the submit it looked like all along. Ctrl-C is never
  offered to it at all: the interrupt outranks it. This is deliberately
  unlike a Tab-only menu that splices
  a lone match without showing one — typing here must never move the buffer on
  its own. Each row also carries the registry's own `Spec::help` line —
  the same sentence `/help` lists — in a dimmer second column, so the popup
  never says less than the listing does; `Menu` drops that column first when
  the terminal is too narrow to hold it.
- `/model`'s model list ranks through `ral_core::text::rank_by` — the same
  `nucleo` matcher the command popup uses, given the `label / model` line each
  row matches by rather than the row itself, so a row and its haystack travel
  together and two providers listing one model name stay two rows. Rows read
  best-match first and, an empty query included, alphabetically within a score.
  The selected row has one full-width highlight, with account labels aligned
  at the right. The wider bezel shows hints for the focused field; routing
  choices sit between fixed arrows, dimmed at either end of the range.
- `/providers` (`tui/providers.rs`) is a terminal-form table of every known
  account in the same bezel: save or clear a key, declare or remove an
  endpoint (a four-field form whose key row ←→ toggles to "checks no key"),
  sign in or out of ChatGPT. A row whose key comes from the environment names
  the variable, in its own colour, with any saved key beneath it; saving or
  clearing a key there says the variable stays in force. `ProvidersOverlay` is display
  and input only; its `Action`s are applied by `providers::input` through exarch's
  `Wallet`, so each change is persisted and live at once. Overlay `Input`s carry
  `Paste` and `Save` (Ctrl-S) for it; the other overlays ignore both. Sign-in
  swaps in the `/login` overlay, which reopens this one when it closes; sign-out is
  local at once (`Bureau::sign_out`), and the issuer's revocation runs on its
  own thread, its outcome landing in the status line as a `Job` that applies
  only while the overlay that spawned it is still the one open
  (`App::overlay_gen`, bumped by every `App::open`).
- `/model`, `/login`, `/providers` and `/rewind` are framed by
  `overlay::draw`: each states only its body's size, and gets one plane,
  margin, double-line bezel, title, hint, padding and translucent shadow
  around it, so the height a body reserves and the rows it lays out come from
  one list. The anchor is centred for the first three, in the frame's bottom-right
  corner over the inert prompt box for `/rewind`, so the transcript it
  previews stays uncovered. The login body drives browser or device OAuth on a background thread,
  receives typed `LoginPhase`s over a channel, and carries the device expiry
  label from the flow rather than reconstructing it in the view. Every body row
  is a `(label, value)` pair over one shared column, and a value too long for it
  wraps instead of clipping; `y` sends the phase's one transcribable value — the URL, or the device code —
  to the clipboard through the `clipboard` crate: native locally, and over
  OSC 52 under ssh, which is how it reaches a browser at the other end. Closing the overlay sets its relaxed
  cancellation flag; browser accept polls it directly, while device polling
  checks it before each bounded request.
- `headless.rs` — one-shot pipe: `--output-format text` suppresses incidental
  root assistant tokens and writes the deliberate `reply` once as ral's
  human-readable value projection; `--output-format json` writes one faithful
  result object from that same reply. Every other event condenses to one line
  on `err`, and the process exits after one seed exchange. The sink projects
  onto an explicit writer pair: `run` is the CLI's headless wrapper, while
  `converse_on` is the conversational projection that keeps streaming tokens
  to a non-CLI host (synod's GUI) one exchange at a time on a parked embedded
  trunk.
  `Headless` takes `Sink::drive` as it comes and keeps one `Blocks` memo per
  source agent; it takes a per-exchange bus, so its async children stay muted.
  It is a display only — the durable `record.jsonl` is written by each
  session's own `AgentLog` (`record/session.rs`), in headless exactly as in the TUI.

`cancel.rs` is the per-agent exchange cancellation layered on ral's interrupt
handling, and `signals.rs` the process's half: the signal dispositions and
`face`, which forwards an ambient cause to every live root of the fleet. Every
agent holds one **sticky** `Token` (an `Arc<AtomicU8>`) for its
whole attend; the attend loop `reset`s it at each genuine exchange boundary. Esc /
Ctrl-C interrupt the *focused tab's* current exchange — never a cascade, never a
subtree kill ([[decisions/260705_cancel-per-tab|cancel-per-tab]]): the focused
tab's own `Weak` upgrades (`Tabs::agent`) and `Agent::interrupt` unwinds that
agent's exchange and eval root. No exarch
shell is ever foregrounded (tool runs are `Run::captured`, the terminal
`Denied`), so a key has no process-wide half: a key interrupts one agent, an
OS signal reaches every root.
The TUI key table keeps UI control separate from cancellation: a modal
overlay's (`/model`, `/login`, `/providers`, `/rewind`) Ctrl-C/Ctrl-D/Esc close it (`input_of`), only
`/quit`/`/exit` end the session, and Ctrl-C/Esc otherwise always drive the
focused tab's non-escalating cancel (`key_action`). A single press stops the exchange /
in-flight HTTP future and unwinds the in-flight eval at its next poll point;
because the path never escalates the signal count, repeated presses cannot
reach ral's third-signal `_exit`
([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).
On Windows the same contract rides `SetConsoleCtrlHandler`: `install` registers
exarch's routine after ral's, so it runs first in the last-registered-first
chain, handles Ctrl-C/Ctrl-Break itself — ral's non-escalating
`request_interrupt`, which signals no process: a tool child hears Ctrl-Break
only from its run's teardown — and reports them handled, so ral's
escalating disposition never ticks for an exchange-cancel; window-close / logoff /
shutdown pass through unhandled to that disposition, the analogue of
SIGTERM/SIGHUP staying on the escalating path. Raw mode disables
`ENABLE_PROCESSED_INPUT`, so Ctrl-C reaches the TUI as an ordinary key event
and nothing re-injects a console event: a `GenerateConsoleCtrlEvent` would
broadcast to the console group and tick ral's escalation counter.
A genuine external signal still routes through ral's one cause-carrying
delivery path ([[decisions/260706_signals-are-causes|signals-are-causes]]).
`boot::face_process_signals` (`boot.rs`) runs once per entry point: it discards
stale ral interrupts and installs the cancel chain over ral's handlers. Every
session shell boots through the installer's two halves,
`boot::engine_runtime` and `boot::engine_root`; `/clear` reboots the seat (`Seat::clear`,
`agent/seat.rs`) and republishes the fresh engine's control sender to the
agent's `InterruptTarget` (`cancel.rs`), so Esc and SIGINT after `/clear` still
reach the new shell. `prompt/host.rs` snapshots the machine (OS, date, cwd,
user, home, git state, exarch's log directory) once at startup for the [[map/exarch/policy|system prompt]].
        - `tui.rs` — thin façade: module declarations, re-exports, `LINGER`, `DEMOTE_IDLE`
        - `tui/app.rs` — the `App` orchestrator: the one key router (`App::key`, then `overlay_input`), `App::open` (the one way an overlay opens, bumping `overlay_gen`), `CommandCtx`, `Job`, `Input`, `KeyAction`, a birth (tab, catch-up), per-kind push methods
        - `tui/tui_loop.rs` — REPL/ui loop: `run` (every root through `spawn_async`, the UI loop until the fleet is empty, then `Fleet::wait_empty`), `Tui`, `ui_loop`, `compose_in_editor`; the loop drains the bus, then the `Job` inbox (work finished off the UI thread, applied on it), polls the open picker, and hands each event to `App::key` / `App::paste` — overlays are states of this one loop, not nested ones
        - `tui/terminal.rs` — terminal lifetime: `TerminalGuard`, raw mode, alt screen, panic hook, stderr redirect, editor hatch
        - `tui/tabs.rs` — session/view lifecycle: `Tab` (birth facts, `Status`, `Life`), `Tabs` as one id-ordered `Vec` with no root, `TabRow` (the matrix's per-frame projection, demotion included), titles, attachment management, the parent climb, `tick`'s retirement to `Life::Gone` past `LINGER`
        - `tui/scrollback.rs` — per-session scrollback as a mirror of the view fold: `Scrollback`, `Scrollback::fact` acting on a `Delta` (`opened`/`grew`/`patched`), the record vocabulary decoded once into `Item`s, `absorb`'s tail rules (an effect walks back to its call, a change joins the run of changes at the tail, everything else grows the tail), `trim`'s head retirement against the fold's own window, `live_tail`, `screen`'s one seam rule, the `Log` transcript writer
        - `record/commit.rs` — the model's stream, cut into records worker-side: `Stream`/`Chopper` into `Display::Answer`/`Thinking` commits
        - `tui/prompt.rs` — prompt editor state: `PromptState`, history, draft, editor request, key input, the live slash-command popup (`refresh_menu`, `menu_key`)
        - `tui/gesture.rs` — the mouse as a transition system: `Cell`, `FrameGeom` (the one place pointer → buffer cell, stamped with the tab it drew, so a gesture never outlives its frame's tab), `Phase` (Idle/Pressed/Dragging/Selected), copy `Toast`, hover. Reads come in as `&Scrollback`; writes go out as an `Effect` (`Scroll`, `CycleBlock`, `Copy`) that `App::apply` runs — the module never mutates a scrollback or touches the terminal
        - `tui/render.rs` — `strips` lays the frame out as a value, `draw` paints it; `paint_selection`, `paint_hover`, `footer_hint`, `emit_tab_title`; the screen-side `Row::into_line` flatten; `draw` records the frame's geometry before painting selection or hover
        - `tui/row.rs` — the transcript row: `Row { gutter, content }`, `rail`/`wash`/`hover`/`plain`/`into_line`, the `RAIL_W` gutter-width invariant
        - `tui/banner.rs` — startup metadata: `SessionInfo`, `session_card` (including the compile-time package version, omitting the disposable scratch path), `legend_panel`, ART/EAGLE constants; `opening` lays the wordmark over the width-matched card as the one rail-free `Chrome::Opening`, neither paying an inset of its own so both start in the column the rail margin already opens
        - `tui/commands.rs` — slash command registry: the `COMMANDS` table of `Spec`s (name, aliases, `Arg`, help), `Command`, `lookup_command`, `command_candidates`, `route_submit`, handler functions
        - `tui/status.rs` — status line: `StateSpan`, `rule_line`, `ctx_ramp`, `wait_bar`, `wait_step`
        - `tui/text.rs` — span-level text primitives: `text`, `wash`, `wrap_line`, `truncate_spans`, `hang`
        - `tui/gauge.rs` — magnitude glyphs: `size_bar`, `spark_glyph`, `grain_run`
        - `tui/dial.rs` — the disclosure dial: `Detail` rungs and the `Part` a click turns
        - `tui/resources.rs` — the `/resources` rows for the accumulators the frontend owns
        - `tui/matrix.rs` — bounded agent-tree matrix: `Matrix` (the one retained value, an agent identity), `Nav`/`nav` reading a key as a gesture, `forest`/`TreeRow` and their connectors, the closed-form `view` and its boundary lines, `neighbour`, `strip`'s justified row projection, `turn_cells`
        - `tui/diff.rs` — a patch as a block: `DIFF_PEEK_ROWS`, `diff_body`'s graded rungs, `header`'s size and grain, the hunk rows numbered against one `DiffCols` gutter, `elision_row`
        - `tui/group.rs` — the `▸` part of a group: `Call`, one burst of `ral` work that its effects join, `aggregate_magnitude`, `body`
        - `tui/line.rs` — line builders turning a typed `Card` into rows: `render_card_unframed`/`render_card_framed`, `thinking_header`, `user_prompt`, `act_row`; no rail glyph, which `Block::railed` sets
        - `tui/rail.rs` — the marginal rail: `RailKind` for shape, hue from `palette::AGENT_HUES`, `value_step`/`lighten` for lightness
        - `tui/md.rs` — markdown to lines over `pulldown-cmark`: `render_md`, `render_thinking`, the `$…$`/`$$…$$` typesetting, `apply_context`
        - `tui/highlight.rs` — ral source coloured by ral's own lexer: `highlight_ral`, `highlight_ral_spans`
        - `tui/fidelity.rs` — coherent degradation: `Fidelity`, turn-level `context_floor` and per-block `echo_delta`
        - `tui/select.rs` — drag-selection geometry in text-area columns: `highlight_range` for painting, `plain_slice` for copying
        - `tui/overlay.rs` — the chrome every modal overlay floats in: `draw`, `body_width`, `Anchor`
        - `tui/picker.rs` — the `/model` tuning overlay: `Picker`, `Pick`
        - `tui/palette.rs` — the TUI colour constants (`CODE_BG`, `SLATE`, `PROMPT_INK`, the agent hues), `span_style`/`role_style` (the one place a card role becomes a hue) and the width vocabulary: `RAIL_W`, `READ_W`, `content_w`, and `Col`, the one column primitive every gutter that holds a cell is built from ([[map/exarch/cards|cards]])
        - `tui/model_picker.rs` — model choice: `PickerSession` (`poll`, `input`; it remembers the tab it opened on, which a pick switches) for the live `/model` overlay and `apply_model_switch`, and `choose` for a launch's picker alone on the screen; list fetching rides [[map/exarch/provider|provider]]'s `Listing`/`Fetches` pumps
        - `tui/login.rs` — the `/login` overlay: `LoginOverlay`, `input`, `apply_login`
        - `tui/providers.rs` — the `/providers` overlay: `ProvidersOverlay`, `Action`, `providers`, `input`
        - `tui/rewind.rs` — the `/rewind` overlay: `RewindOverlay`, `rewind`; `Scrollback::render_cut_window` positions the preview and returns the cut row that `render::draw` ghosts from
