---
generated_at_commit: fc497f7577bfce957329627870a1f9292520b782
generated_at_date: 2026-10-28
covers_paths: [exarch/src/bus.rs, exarch/src/bus/, exarch/src/record.rs, exarch/src/record/, exarch/src/tui.rs, exarch/src/tui/, exarch/src/headless.rs, exarch/src/boot.rs, exarch/src/agent/engine.rs, exarch/src/agent/handle.rs, exarch/src/signals.rs, exarch/src/prompt/host.rs]
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
  outside the abandoned tree; `/clear` discards the queue and cancels the old
  incarnation, fencing future batches too.
- Releasing a result's receipts and taking up a message's `Owed` are atomic
  with pop. Busy is receipts outstanding for the current work; each reply
  pays the debts it covers, and a dropped debt pays `Cancelled`.
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
  recall, so Up withdraws only text it can display.

## Outbound: records and live edges

**Recording precedes publication, and completion is not channel state.**

- `record/seam.rs::Emitter::emit` appends each `Record` to `record.jsonl`
  and publishes its witnessed `Recorded<Record>` under the writer's mutex.
  `Protocol`, `Display` and `Forensic` are disjoint durable classes;
  `Transient` has no sequence or durable form.
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
  Missing remote teardown acknowledgment makes the conversation terminal.
  The next job, “Stopped.” and Synod's closing folder walk cannot overtake
  those writes or final output
  ([[decisions/260806_exchange-ends-at-fleet-quiescence|job settlement]]).
- `end_fleet(self)` ends the whole shared fleet, drops the owner, then waits
  on the detached `Census`, retaining no launch or factory. Synod recovers
  its machine only afterwards ([[map/synod|synod]]).

## The session record and its mirrors

**A transcript is a projection of one record, never a second log.**

- `record/session.rs::AgentLog` owns the model `Context` and record seam.
  `render_messages` builds the provider view on call; it is memoised nowhere.
  `quiesce` leaves a fresh prompt admissible however attendance ends
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
- `Transient::Died` remains the ordered presentation death cue; buffered
  records preceding it must still draw. Descendants retired by `/clear`
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
  OSC 52 over ssh.
- `overlay.rs` owns the shared bezel. Model, login, provider and rewind
  overlays run in the one UI loop; background results apply only to their
  originating overlay generation. `/rewind` previews and posts to the tab
  it opened on, not whichever tab later acquires focus.
- The status rule times state transitions rather than arriving events;
  usage and context figures use the same provider data as other frontends.
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
  `/quit` cancels the fleet root. `/clear` checks rebootability before
  abandoning work/queue and replacing its engine. No target is republished.
- Overlays consume their close gestures without cancelling work; matrix
  Esc leaves navigation. Raw mode makes a TUI key local, not a process signal.
  On Windows the console routine handles Ctrl-C/Ctrl-Break before ral's
  escalating disposition; it never broadcasts a new console event.
- Repeated interactive interrupts never tick ral's force-exit ladder.
  External shutdown retains that ladder
  ([[internals/cancellation|cancellation]]).

## One-shot headless

`headless.rs::run` uses a per-exchange bus with muted child presentation,
not muted recording. It seeds one returning root, projects the deliberate
reply once to stdout (ral text or faithful JSON), and condenses progress to
stderr. `Headless` keeps one `Blocks` memo per source; totals come from the
bus meter, including muted children. A host needing multiple human-opened
jobs instead supplies its own `Sink` to `SettledConversation`.
