---
generated_at_commit: 0e83863c
generated_at_date: 2026-10-29
covers_paths: [exarch/src/bus.rs, exarch/src/bus/, exarch/src/record.rs, exarch/src/record/, exarch/src/tui.rs, exarch/src/tui/, exarch/src/headless.rs, exarch/src/boot.rs, exarch/src/agent/engine.rs, exarch/src/agent/handle.rs, exarch/src/signals.rs, exarch/src/prompt/host.rs, vendor/edtui/, core/src/external_editor.rs, core/src/io/bounded.rs]
---

# Map: exarch / frontend

**The agent and its frontend meet at one outbound event stream and one
inbound inbox.** The owner is `Agent`; every frontend addresses it through
`AgentHandle`, never a retained owner or direct mailbox
([[map/exarch/agent|agent]],
[[decisions/261028_one-scope-tree-one-inbox|one-scope-tree-one-inbox]]).

## Inbound: one inbox

`bus/inbox.rs` owns the queue, work, exchange, job and status under one
mutex and condvar; `bus/post.rs` owns delivery payloads and `Next`.

- Private `Post` is human text, `Read`, `Rewrite`, or a scoped payload.
  `Posts` drops scoped posts whose provenance is cancelled before every
  observation, under the lock. Human text and commands are unfenced. A mailbox carries destination
  and vouching `Scope`; it never refreshes provenance at push.
- Results are vouched by parental work, wakeups by arming work, nudges by
  their exchange, worker batches by engine incarnation, and messages by the
  sender's containing scope. Stop preserves worker batches and messages from
  outside the abandoned tree. A committed `/clear` discards its captured
  queue boundary and cancels the old incarnation, fencing future batches
  while preserving later human input.
- Taking up a message is plain; a parent's follow-up is `Scoped::FollowUp`.
  Busy is a Working child of the current work; finish queues the value-free
  notice with the status under the parent's lock
  ([[decisions/261029_a-child-is-a-status|a-child-is-a-status]]).
- `Inbox::draw` checks life and stopped job, computes the park verdict under
  the lock, then pops before honouring `Quiesce`. It mints an owned
  `Opening` at pop. Accepted arrival `open`s it; a declined claimed-worker
  completion drops it, preserving the current continuation.
- `Inbox::drain` takes live deliveries and reads at a continuing tool
  boundary, stopping at a rewrite; a cancelled exchange drains nothing.
  Owner-side `Agent::{read, rewrite}` (`agent/command.rs`) run commands in
  typed order. A `/branch` therefore sees precisely the context before it.
- A drawn human prompt is recorded before polling cancellation; a racing
  Stop or Esc starts no provider round but cannot silently erase that text.
  A staged reply abandons work with `Keep` before draining or reminders.
- Wakeups coalesce by never-reused id, nudges replace queued nudges, and
  trailing human posts join with a newline. Fencing dead posts re-coalesces
  newly adjacent human posts. Other sources queue uncapped;
  `source_depths` observes depth, not admission quotas.
- Owner abandonment clears `ScheduleRegistry` after unlocking. Timer entries
  hold weak work mailboxes and retire when work dies. Inbox may read the
  registry's leaf lock; `fire_at` releases it before posting.

`tui/commands.rs::route_submit` classifies input once:

- Immediate UI commands (`/help`, `/copy`, `/model`, …) act on the UI thread.
  `/quit` directly ends the fleet root without requiring an inbox consumer.
- Reads (`/new`, `/branch`, `/context`, `/resources`) enter through
  `AgentHandle::post_read` and run at the next tool or exchange boundary.
- Rewrites (`/clear`, `/close`, `/evict`, `/rewind`) enter through
  `post_rewrite` and hold subsequent input until the exchange boundary.
- Other text, including unknown slash-shaped lines, enters through `steer`.
  Queue observation and recall use `queued` / `recall`.
  `PromptState::edit_queued_prompt` checks draft/history state before invoking
  recall, so Up withdraws only text it can display. `Mailbox::recall` fences,
  then withdraws every queued human post under the inbox lock, across
  intervening reads, rewrites and scoped posts, which stay queued in order;
  an ended agent recalls nothing. It is not a barrier-aware trailing-run recall.

## Outbound: records and live edges

**Recording precedes publication, and completion is not channel state.**

- `record/seam.rs::Emitter::emit` appends each `Record` to `record.jsonl`
  and publishes its witnessed `Recorded<Record>` under the writer's mutex.
  `Protocol`, `Display` and `Forensic` are disjoint durable classes;
  `Transient` has no sequence or durable form. The writer owns a `File`
  directly. The first uncommitted append (write, encoding or append-lock
  poison), or durable record refused by the fold, latches `Unavailable`
  across clones and ends the life supplied by `Emitter::bind`. `emit` returns
  that failure; `check_available` observes it; `emit_or_end` leaves it to the
  owner. Producers never report it. The owner's terminus announces it once
  as `Transient::Fault` on the independent fleet sink and makes it the outcome.
- `bus/signal.rs::Signal` is `Fact`, `Born`, or `Transient`, addressed by
  `AgentId`. `Emitter::child` publishes birth before that emitter can send
  any work. `bus/sink.rs::Sink` consumes `fact` / `transient`.
- `bus/emitter.rs::FleetBus` owns the channel and usage meter. `pump`
  (`bus/sink.rs`) runs an owner on a scoped thread and drains into its sink.
  An explicit completion flag ends the pump even if background producers
  keep the bus nonempty. A recovered panic is reported distinctly.
- `bus/channel.rs` coalesces adjacent same-agent `Token` / `Thinking`
  text and replaces adjacent `State`; facts and other edges remain ordered.
  `MERGE_TEXT_CAP` bounds a merged text run, with a fault marker for elision.
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
`Agent::arrive` suppresses redundant wakeups for already-claimed workers.

## Embedded conversation: jobs

**`SettledConversation` owns its root; the host holds its address.**
`headless.rs` shares the ordinary attend loop with embedded hosts.

- `new(root)` consumes the `Agent` and creates a conversation-lived bus.
  Self-scheduling and reset-resume are refused. `handle`, `severance` and
  `run_dir` expose steering and diagnostics without lending the root.
- `await_job` opens a `Job` under the root inbox lock only when human text
  is queued; `None` means life has ended. A worker batch alone cannot open
  a job. Dropping an unattended job closes it.
- `run(job, sink)` attends at most once through fleet quiescence, returning
  `JobEnd { id, stopped, outcome }`. Replied resident helpers are not busy;
  signals between jobs wait for the next drain.
- `AgentHandle::stop(JobId)` atomically marks only the matching running,
  unstopped job and abandons its work with `Recall`. Stale or duplicate Stop
  returns `None` and changes nothing. A stopped job pops nothing more; new
  human text remains queued for the next job.
- Every job waits through `Fleet::wait_settled` for ended helpers' teardown
  inside the output pump before closing; if Stop wins that race, it settles again.
  Missing remote teardown acknowledgment or unavailable session recording
  makes the conversation terminal. `SettledConversation::run` concludes after
  its pump: an unavailable writer is announced in a second pump however
  attendance ended, and the helpers whose lives it ended settle there.
  The failure overrides the attended outcome, as at every terminus. That
  census report supersedes the first. `JobEnd.outcome` and the census
  join failures one per line. A settlement failure severs once, after the
  pumps, which ends the root's life with its engine. The next job, “Stopped.”
  and Synod's closing folder walk cannot overtake those writes or final output
  ([[decisions/260806_exchange-ends-at-fleet-quiescence|job settlement]]).
- `end_fleet(self)` ends the whole shared fleet, drops the owner, then waits
  on the detached `Census`, retaining no launch or factory. Synod recovers
  its machine only afterwards ([[map/synod|synod]]).

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
  mutex, then streams beyond a `Cursor` outside it. Records arriving after
  that extent take the channel. `record/replay.rs` owns replay/refusal.
- `record/view.rs::Blocks` folds each witnessed fact; `Delta` reports
  `Opened`, `Grew`, `Patched`, `Rewound`, `Cleared`, or `Quiet`.
  `tui/mirror.rs::Mirror` maintains one rendered block per retained incident;
  `Scrollback::fact` catches up from the log at birth or a sequence gap.
  Duplicate facts fold once. Headless/Synod sinks narrate rather than catch up.
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
  and routes input through `App::key` / `paste`. It ends only when every
  tenancy has been released. `terminal.rs` owns raw
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
- `record/commit.rs::Stream` cuts prose/thinking at newlines; the view draws
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

**A key interrupts exchange; Stop abandons work; end cancels life.**

- TUI active-exchange Esc / Ctrl-C call `AgentHandle::interrupt` on the
  focused address, cancelling exchange and pending opening with
  `Interrupted`. Helpers and loose ral workers survive.
- `signals.rs::face` forwards ambient SIGINT to roots' exchanges and
  shutdown to the fleet root. `boot::face_process_signals` installs ral's
  handlers then exarch's non-escalating interrupt disposition once at entry.
- Provider waits poll the exchange; `Agent::ral` passes it to
  `dispatch_to_report`, whose own dispatcher strikes its own id. Cancellation
  never writes to a transport from a cancelling thread.
- `AgentHandle::end`, `/close`, lease lapse and owner drop cancel life;
  `/quit` cancels the fleet root. `/clear` prepares a replacement and commits
  `Cleared` before abandoning old work/input or replacing its engine.
  Failed preparation preserves live resources. A failed commit ends the
  session and certifies the unused prepared engine, leaving the current
  engine, record and registers unchanged. A successful clear retains input
  posted after its captured boundary. No target is republished.
- Overlays consume their close gestures without cancelling work; matrix
  Esc leaves navigation. Prompt searches consume their own Esc / Ctrl-C, and
  Ctrl-C copies a prompt selection when present; only after these owners
  decline do the keys interrupt. Ctrl-C never changes draft text. Raw mode makes a TUI key local, not a process signal.
  On Windows the console routine handles Ctrl-C/Ctrl-Break before ral's
  escalating disposition; it never broadcasts a new console event.
- Repeated interactive interrupts never tick ral's force-exit ladder.
  External shutdown retains that ladder
  ([[internals/cancellation|cancellation]]).

## One-shot headless

`headless.rs::run` uses a per-exchange bus with muted child presentation,
not muted recording. Its pumped closure attends; conclusion follows the
pump. An unavailable writer is announced once in a second pump however
attendance ended. It seeds one returning root, projects the deliberate reply
once to stdout (ral text or faithful JSON), and condenses progress to
stderr. `Headless` keeps one `Blocks` memo per source; totals come from the
bus meter, including muted children. A host needing multiple human-opened
jobs instead supplies its own `Sink` to `SettledConversation`.
