---
generated_at_commit: e5a23909
generated_at_date: 2026-10-29
verified_at_commit: e5a23909
verified_at_date: 2026-10-29
covers_paths: [exarch/src/bus.rs, exarch/src/bus/, exarch/src/record.rs, exarch/src/record/, exarch/src/tui.rs, exarch/src/tui/, exarch/src/headless.rs, exarch/src/host.rs, exarch/src/boot.rs, exarch/src/agent/engine.rs, exarch/src/agent/handle.rs, exarch/src/signals.rs, exarch/src/prompt/host.rs, vendor/edtui/, core/src/external_editor.rs, core/src/io/bounded.rs]
---

# Map: exarch / frontend

**The agent and its frontend meet at one outbound event stream and one
inbound mailbox.** The owner is `Agent`; every frontend addresses it through
`AgentHandle`, never a retained owner or direct mailbox
([[map/exarch/agent|agent]],
[[decisions/261029_an-agent-is-a-thread|an-agent-is-a-thread]]).

## Inbound: one mailbox

**`bus/mailbox.rs` linearises queue, published status, jobs and Stop.**

- `AgentMailbox` is the owner end; `AgentAddress` shares its queue and life.
  Inputs are human text, messages, child reports/departures, epoch-stamped
  wakeups, generation-stamped worker batches and commands.
- `next` selects admissible input and mints its exchange lease under life.
  `Agent::admit` checks stale epochs/generations and claimed workers before
  `begin` records `JobOpened`. A declined input opens no job.
- Human fragments coalesce only while adjacent. `drain` hears ordered input
  through the first rewrite while the exchange is live. A drawn human line
  is recorded before cancellation is polled.
- `Free` admits any input at rest; `Gated` consumes one release per job;
  `Parent` admits the human and parent, refusing resting peer messages.
  Commands and child reports/departures are quiet and never held.
- Pending helpers owe their parent or have ended without joining. A report
  queues `Child` before clearing that obligation; `Gone` joins quietly.
  Published status serves views and `read`, not a second shared register.
- `post_wakeup_with` orders coalescing, registry updates/terminal retirement
  and enqueue in one mailbox transaction. Timers hold a weak shared address.
  Mailbox → registry is the lock order. Returning agents wait on schedules;
  conversing schedules may open later jobs. Reports and clear invalidate epochs.
- Clear drops queued reports, wakeups and batches, retaining all human text,
  messages and commands. Generation checks reject outgoing-engine batches.

`tui/commands.rs::route_submit` classifies input once:

- Immediate UI commands (`/help`, `/copy`, `/model`, …) act on the UI thread.
  `/quit` directly ends the fleet root without requiring a mailbox consumer.
- Reads (`/new`, `/branch`, `/context`, `/resources`) enter through
  `AgentHandle::post_command(Command::Read)` and run at the next tool or exchange boundary.
- Rewrites (`/clear`, `/close`, `/evict`, `/rewind`) enter through
  `post_command(Command::Rewrite)` and hold subsequent input until the exchange boundary.
- Other text, including unknown slash-shaped lines, enters through `steer`.
  Queue observation and recall use `queued` / `recall`.
  `PromptState::edit_queued_prompt` checks draft/history state before invoking
  recall, so Up withdraws only text it can display. `AgentAddress::recall`
  withdraws every queued human post under the mailbox lock, across
  intervening reads, rewrites and other inputs, which stay queued in order;
  an ended agent recalls nothing. It is not a barrier-aware trailing-run recall.

## Outbound: records and live edges

**Recording precedes publication, and completion is not channel state.**

- `record/seam.rs::Emitter::commit` appends one commit to `record.jsonl`
  and publishes it whole under the writer's mutex; `emit` commits one record.
  `Protocol`, `Display` and `Forensic` are disjoint durable classes;
  `Transient` has no sequence or durable form. The writer owns a `File`
  directly. The first uncommitted append (write, encoding or append-lock
  poison), or durable record refused by the fold, latches `Unavailable`
  across clones and ends the life supplied by `Emitter::bind`. `emit` returns
  that failure; `check_available` observes it; `emit_or_end` leaves it to the
  owner. Producers never report it. The owner's terminus announces it once
  as `Transient::Fault` on the independent fleet sink and makes it the outcome.
- `bus/signal.rs::Signal` is `Commit`, `Born`, or `Transient`, addressed by
  `AgentId`. `Emitter::child` publishes birth before that emitter can send
  any work. `bus/sink.rs::Sink` consumes `fact` per record within one
  `accept`, and `transient`.
- `bus/emitter.rs::FleetBus` owns the session-lived channel and usage meter.
  `host.rs::Host` launches joined roots; frontends receive their signals
  without driving agent work. Completion comes from records or terminal
  death, never channel emptiness.
- `bus/channel.rs` coalesces adjacent same-agent `Token` / `Thinking`
  text and replaces adjacent `State`; facts and other edges remain ordered.
  `MERGE_TEXT_CAP` bounds a merged text run; `Transient::Elided` reports
  presentation loss without turning it into a session fault.
  `BusReceiver::{depth, bytes}` supply resource probes.
- `AgentState` is live presentation state (`Ready`, `AwaitingModel`,
  `Evaluating`, `Evicting`, `WaitingOnAgents`), never model context.
  The record layer knows no bus: it publishes through `Publish`; bus knows
  no agent ([[invariants/exarch-is-a-dag|exarch-is-a-dag]]).

`shell_eval/surface.rs` decodes the shell's surface vocabulary once.
Observations, notices, cards, pins and worker completions record through the
same seam; card interpretation is [[map/exarch/cards|cards]]'s.
The transcript calls workers *errands*: birth and completion carry the same
worker id/title. `surface_notice` (`bus/post.rs`) words the model notice with
elapsed time, actual ending and how to use `await` / `workers`.
`Agent::admit` suppresses redundant wakeups for already-claimed workers.

## Host and observed jobs

**`Host` owns roots and their bus; hosts gate, never drive.**

- `Host::{found, resume}` constructs and launches roots. `roots`, `feed`,
  `usage` and `run_dir` expose addresses, signals and accounting. `end` and
  `Drop` end the fleet and join every root, whose teardown joins helpers.
- Every agent records a busy period: `JobOpened`'s commit number is `JobId`;
  `JobSettled { job, stopped }` closes it. Opening/registration, settlement
  and addressed Stop share the mailbox lock.
- `AgentHandle::stop(JobId)` marks the matching open job and cancels its latest
  exchange with `Interrupted`; `stop(job)` also recalls human text oldest first. Stale ids
  and rest are no-ops. Helpers keep working; reports and subsequent input
  can continue the same job, whose stopped mark persists.
- Settlement requires an empty queue and no pending helper; a helper whose
  life ended before settlement is joined first. A helper ending after it can
  fail Synod's walk and report. A returning agent also waits for armed
  schedules.
- Synod uses `Admission::Gated`: release after the baseline and after each
  closing walk admits the next job. Worker batches, wakeups and messages can
  open it as human text does. The host observes records and takes the walk;
  it never lends a root to a job driver ([[map/synod|synod]]).
- `EngineSevered` records engine loss; `TeardownFailed` records uncertified
  engine end. Failures propagate through joins to `Host::end`. Synod treats
  teardown failure as terminal, not permission for a folder account.

## The session record and its mirrors

**A transcript is a projection of one record, never a second log.**

- `record/session.rs::AgentLog` owns the model `Context` and record seam.
  `render_messages` builds the provider view on call; it is memoised nowhere.
  Fallible `quiesce` commits owed protocol repairs before advancing the
  fold. Success leaves a fresh prompt admissible; failure makes the owner
  explicitly unavailable and reaches the host as a failed session
  ([[internals/session-record|session-record]],
  [[invariants/turn-ends-ready|exchange-ends-ready]]).
- `record/log.rs::Log::since` fixes extent and read handle under the writer
  mutex, then streams commits beyond a `Cursor` outside it. Commits arriving
  after that extent take the channel. `record/replay.rs` owns replay/refusal.
- `record/view.rs::Blocks` folds each witnessed fact; `Delta` reports
  `Opened`, `Grew`, `Patched`, `Rewound`, `Cleared`, or `Quiet`.
  `tui/mirror.rs::Mirror` maintains one rendered block per retained incident;
  `Scrollback::commit` catches up from the log at birth or a commit gap.
  Duplicate commits fold once. Headless/Synod sinks narrate rather than catch up.
- Resume catches up each root only to its recorded pre-run extent, draws
  the resumed note there, then catches up the new run. The bus's usage meter
  plus resumed usage supplies totals, independent of views.
- A held session directory contains `record.jsonl` alone.
  `tui/export.rs::export_to` replays it through `Mirror` and a `Format`,
  creating a new destination rather than overwriting. `/export` and
  `exarch export` share this door.
- `Protocol::Cleared` and `Rewound` withdraw context in the same file.
  Resource readings are live diagnostic cards, not pressure histories.

## TUI: a view per address

**The TUI owns presentation, never agent liveness.**

- `tui/tui_loop.rs` drains the session-lived bus and UI jobs, polls overlays
  and routes input through `App::key` / `paste`. Producer completion is
  `Host::is_finished`: the directory is empty and every retained root thread
  is finished. It performs an uncapped final drain and final frame;
  `Host::end` still owns joins and failure collection. `terminal.rs` owns raw
  mode, alternate screen, bracketed paste, mouse capture and panic restoration.
- `tui/tabs.rs::Tabs` is an id-ordered forest of birth facts and
  `Status` / `Life`; there is no special root tab. `Tabs::agent` resolves an
  `AgentHandle` through fleet names and checks id. Retained addresses hold
  no engine and cannot prolong teardown.
- `Transient::Died` is the final owner presentation cue, after availability
  classification, failure reporting and retirement. Terminal delivery uses
  the owner's fleet sink even when append state is poisoned; the poisoned
  log is never recovered. Buffered records preceding death must still draw. Descendants retired by `/clear`
  linger through the same path. After `LINGER`, scrollback, render memos and
  pins drop; held log directories remain for export.
- Focus and matrix navigation are presentation identities. `matrix.rs`
  derives rows/connectors from the forest before clipping; idle demotion is
  computed from the inbox exchange clock, never stored as lifecycle state.
- `scrollback.rs` owns viewport and live tails; `block.rs`, `group.rs` and
  `dial.rs` own graded disclosure. The `Blocks` window bounds retained
  incidents; mirror trimming follows the fold rather than a second policy.
- `row.rs::Row { gutter, content }` keeps marginal ink out of copying.
  `render.rs` lays out at the known width, records gesture geometry, and
  brackets terminal writes in synchronized updates. `gesture.rs` returns
  effects over immutable views; its caller applies them.
- `rail.rs`, `line.rs` and `group.rs` bind shape to kind, hue to producing
  agent, luminance to magnitude/figure-ground. Machine output is recessed;
  human prompts have their own neutral fence. Cards use one mark interpreter.
- `record/stream.rs::Stream` cuts prose/thinking at newlines; the view draws
  the uncommitted tail in the block that will absorb it. `Boundary` retires
  abandoned tails. `md.rs` shares ral highlighting with tool panels, renders
  math through `tui-math`, and falls back to literal source on refusal.
- `prompt.rs` and `prompt/editor/` own drafts, history, undo and the fuzzy
  slash-command popup. Completion is one undoable edit; recalling another
  draft starts a fresh undo baseline. Clipboard routing is native locally,
  OSC 52 over ssh. `PromptState` holds the draft, guarded recall, searches
  and the live popup (`refresh_menu`, `menu_key`), over an injected
  `History`; `prompt/search.rs` traverses matches; `prompt/editor.rs` owns
  the editor wrapper, with
  `editor/{editing,navigation,geometry}.rs` for native transactions, visual
  movement and height, and `editor/completion.rs` for popup geometry.
- **Composer input has one owner at a time.** The UI loop admits only key
  presses — Windows also reports releases; under the
  `DISAMBIGUATE_ESCAPE_CODES` flags alone no terminal reports repeats — so
  nothing downstream inspects a key's kind. `App::key` routes each press
  once, in order: the open overlay; prompt-selection copy, on a steerable tab
  outside the matrix; matrix navigation; a live search
  (`PromptState::search_key`: editor-native Vi search, else Ctrl-R history
  search, which Ctrl-R opens); the popup, or Tab into the matrix; chords;
  then interrupt, submit or edit. Only unmodified Enter submits. Watch-only
  tabs cannot edit, paste, accept completion or launch the editor. `Ctrl-G`
  and `Ctrl-X Ctrl-E` ask the UI loop to compose externally. The armed
  `Ctrl-X` is `App`'s `Prefix`, taken by the next key; focus, modal, paste,
  mouse, resize and suspension boundaries drop it.
  `MouseOwner` alone records a left gesture from press through release;
  crossing the prompt edge never transfers a drag. A prompt press yields a
  `Grip`, the editor's selection anchor, held only in `MouseOwner::Prompt`;
  a key or paste ends that ownership, since it may rewrite the draft the
  anchor indexes. Terminal ownership is taken only on a shifted left press;
  any shifted event leaves selection to the terminal. Ctrl-C copies an editor
  selection before considering exchange cancellation, and copy errors are
  reported. Selection clears only after a confirmed clipboard write; failure
  retains it for retry, while success uses the transient copy toast.
- `prompt/history.rs::History` owns the entries, their store and the arrow
  browse position; replacing the entries ends the browse. `tui_loop::run` opens it
  and injects it into `PromptState`. The store is shared
  `$XDG_STATE_HOME/exarch/prompt-history.json`, private and under a
  nonblocking file lock, read through `ral_core::io::read_bounded`. It
  retains at most 500 entries and 1 MiB of prompt bytes, merges other
  sessions' entries on save, and reports failures without losing
  submission. An unreadable startup history leaves an unstored
  `History` and a notice rather than being overwritten. `history_prev` /
  `history_next` either move or report refusal; arrow recall requires Insert
  mode, an empty draft or unchanged history entry, and the absolute buffer
  start/end. Queued recall additionally requires an empty draft and no
  active history browse/search.
- `prompt/search.rs` and `PromptState::search_key` own Ctrl-R search:
  the query and footer preview leave the draft, cursor and undo untouched
  until acceptance; a click in the box is claimed but inert. An empty query
  previews the newest entry; Ctrl-R/Up and Ctrl-S/Down traverse matches.
  Enter adopts without submitting; Esc/Ctrl-C cancel without cancelling the
  exchange. Paste goes to the owning search query, not the draft.
  Paste normalizes CRLF/CR to LF; the large-paste footer is
  only a byte/line summary, never a placeholder that is expanded at submit.
- `terminal.rs::edit_text_in_editor` runs the external editor over the
  shared contract in `ral_core::external_editor`: `EditorCommand::select`
  takes nonblank VISUAL, then EDITOR, then the platform default, splitting
  quoted argv without a shell (`shlex` on Unix, `winsplit` on Windows) and
  refusing NUL; `admit_draft` and `read_draft` cap input and readback at
  `MAX_DRAFT_BYTES` (16 MiB) and keep trailing newlines. The secret draft is
  a consuming `DraftFile` in exarch's cache `editor-drafts` directory,
  outside its writable scratch subtree; `DraftFile::edit` runs the editor
  under `catch_unwind` so `remove(self)` happens exactly once, even on
  unwind, and `after_cleanup` never drops a removal failure. Failure keeps
  the draft and reports a diagnostic; terminal restoration failure exits the
  UI loop. `tui_loop::compose_in_editor` adopts successful text into a
  fresh draft and repaints, never submits it.
- **Patched native edtui is the sole undo owner.** `editor/editing.rs` uses
  its transactions and explicit range/movement primitives for Insert-mode
  grapheme edits, kill/yank and repeat recording. Native history is bounded
  by steps and snapshot bytes; no facade history or second Vi parser exists.
  Pending native commands retain their character arguments, including `u`.
  `vendor/edtui/PATCHES.md` records the local 0.11.7 patch rationale: expose
  grouping without rebuilding editor state, and share extended-grapheme
  geometry across drawing, height, cursor placement and mouse hit-testing.
  Native action execution also snaps cursor boundaries during replay, not
  just after live dispatch. `editor/geometry.rs` consumes this geometry;
  `editor/navigation.rs` moves Insert-mode Up/Down and Ctrl-P/N through visual
  rows, retaining the preferred display column across height-only layout changes.
  Characterwise selection extraction removes exactly its projected byte range,
  leaving unselected newlines intact. **This is not full native Vi
  grapheme-count semantics:** storage and Vi counts remain scalar-indexed.
  Codex-derived host algorithms and their Apache-only boundary are in `NOTICE`;
  the separately patched vendor crate remains MIT-licensed.
- `paste_burst.rs::PasteBurst` buffers rapid unbracketed character runs and
  guards a following Enter, emitting lossless paste chunks instead of submit
  gestures. This is a timing heuristic, not proof of paste: sufficiently slow
  unbracketed input is indistinguishable from typing. Bracketed paste remains
  explicit; no placeholder text is expanded into instructions at submission.
  Bursts form only while `App::takes_text`. `tui_loop::guard_owner` wraps
  every act that may move input (a fact, a death, a job, a tick, a delivered
  key) and finishes the burst when the `InputOwner` changes, so held text
  never crosses owners. A deterministic integration test routes burst output
  through `App`, checking that the guarded Enter inserts a newline rather
  than submitting.
- `overlay.rs` owns the shared bezel. Model, login, provider and rewind
  overlays run in the one UI loop; background results apply only to their
  originating overlay generation. `/rewind` previews and posts to the tab
  it opened on, not whichever tab later acquires focus.
- The status rule times state transitions rather than arriving events.
  `StateSpan` is active activity or `Ended`; retirement cannot be overwritten
  by a later Ready or streaming update. A recording failure is transient
  error chrome followed by ordinary owner retirement, not a fake ready tab.
  Usage and context figures use the same provider data as other frontends.
  `tui/resources.rs` adds frontend-owned probes to the owner's resource card.

## Keys and signals

**Stop cancels the exchange; end cancels life.**

- A TUI tab tracks `JobOpened` / `JobSettled`. Esc / Ctrl-C on an active
  tab calls `AgentHandle::stop(job)`; with no matching job no Stop occurs.
  An unclaimed Esc reaches the editable prompt, including Vi's mode change;
  Ctrl-C never edits. Helpers and loose shell workers survive. The TUI has
  no `/stop` command.
- `PromptState::restore_stopped_prompts` prepends every recalled prompt to
  the live draft, preserving text even during history browsing or outside
  insert mode. Stop never uses Up's guarded `edit_queued_prompt` path.
- Overlays, matrix navigation, searches and selected-text copying consume
  their gestures first. A successful Ctrl-C copy retains the draft; only an
  unclaimed key reaches Stop. Raw-mode keys are local, not process signals.
- `signals::face` forwards SIGINT to each root's `stop_current`, shutdown to
  `Fleet::end`. Windows handles Ctrl-C/Ctrl-Break before ral's escalating
  handler; repeated interactive gestures never tick the force-exit ladder.
- Provider waits poll the exchange; ral dispatchers strike their own id.
  Cancelling threads never write to a transport.
- `end`, `/close`, helper idle timeout and owner drop end life; `/quit` ends
  the fleet. Clear prepares first, joins helpers, commits and replaces the
  engine, preserving human/message input. Failed preparation changes nothing;
  failed commit ends life and retires the unused prepared engine.

See [[internals/cancellation|cancellation]] for scope propagation.

## One-shot headless

**`headless.rs::run` observes a returning root through `Host::feed`.** It
steers the seed, folds only the root's records into one `Blocks` memo, and
finishes on root `Forensic::Status` or `Died`. Helper facts do not print;
helper faults remain visible and usage includes the fleet. `Replied(v)`
writes faithful ral text or JSON to stdout; Failed, Stopped or death without
a reply fail honestly. `lib::run` ends the host afterwards and includes
teardown failures in the result.
