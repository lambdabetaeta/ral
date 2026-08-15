---
status: accepted
---

# One seam, one log

**Everything a session records crosses one seam, once, into one durable log —
`sessions/<n>/record.jsonl` — and every durable artifact is a fold of that
log.** This inverts which artifact is authoritative: the record is the
session, and the model's context, the resumed scrollback, and the rendered
`user.log` are projections of it. `events.jsonl` retires. The inversion is
**complete for the model view and for resume, and deliberately incomplete for
live display**, which still rides the legacy `Kind` stream — the open half is
enumerated at the bottom, blocker by blocker, rather than rounded up to done.

## The founding bug

`exarch --resume` restored the model's memory of a session and none of the
user's: `events.jsonl` replayed into chat history while the TUI opened on an
empty scrollback, because the two artifacts were written on two unrelated
paths — `AgentLog`'s record family under a mutex on the attend thread, and
the deliberately lossy bus channel the TUI projects. That is not a missing
feature; it is two authorities for one session — the debt
[[decisions/260623_recording-follows-the-event|recording-follows-the-event]]
had already named **two-record unification**, with the bus as the blocker.

## The decision

- **One vocabulary, three classes.** `Record { Protocol, Display, Forensic }`
  (`exarch/src/record.rs`): verbatim provider payloads the model fold needs;
  *commits* — chopped, coalesced, reduced before the seam, so what is recorded
  is what the user saw — that the view fold needs; and breadcrumbs neither
  fold projects but worth keeping. The classes are sealed; a fourth cannot be
  minted outside the module. `Transient` (deltas, the thinking seat, chrome)
  is the channel's disjoint other passenger and never touches the file —
  journaling a delta and publishing an unrecorded fact are both type errors.
- **Append-then-publish is ownership.** `record::Emitter::emit` is the only
  publisher; the fleet-channel sender lives *inside the log's mutex*
  (`record/log.rs`), so a record can only be published while its append is
  held and channel order is log order by construction. The publisher is
  attachable — the log outlives any one bus — and weak, so an absent consumer
  costs nothing: the record is already durable, and a pressured consumer's
  escape is the file, since every fact carries a `Seq`. A failed append is a
  session error, never a shrug.
- **The model view is a genuine fold.** `record/model.rs` folds `Protocol`
  records alone — the same `step`, applied inline on the attend thread after
  each `emit` and by `record/replay.rs` from disk. `AgentLog`'s **only**
  session state is this fold's `Memo`; the `events.jsonl`-backed engine is
  deleted whole. The 260812 laws migrate intact: no recorded fact is ever
  removed, `/clear` rotates rather than truncates, resume quarantines a torn
  tail, and the ledger indexes the protocol subsequence per record through
  each `Stamp`'s byte range, never by contiguous run.
- **The view fold exists and follows the same law.** `record/view.rs` folds
  `Display` commits and the `Forensic` rows a scrollback draws into `Blocks`;
  `Block`'s constructor is private, so `Blocks` is unforgeable — only
  `View::step` may push a row, and a printer draws blocks it cannot mint.
  What a frontend builds from the vocabulary is its own fold, exhaustive over
  `Record` and `Transient` under `deny(clippy::wildcard_enum_match_arm)`, so a
  new variant breaks every frontend that must decide about it; `replay`
  carries the `fold == memo` proof once, generically.
  A record no fold recognises is a `Refusal` that refuses the session —
  the display vocabulary is a designed, validated protocol, not a byproduct.
- **Commits are authored worker-side.** `record/commit.rs` owns the
  coalescers that used to live in the frontend: the `Chopper` cuts the
  assistant's delta stream at fence-safe paragraph breaks into
  `Display::Answer` commits, and `SurfaceBuffer` (moved whole from
  `tui/surface.rs`) groups observations into one `Display::ObservationGroup`
  and coalesces diff hunks — so the screen still authors nothing, and 260623's
  maxim survives the move: recording a commit is not the worker narrating the
  screen, it is a designed protocol recorded at the seam.
- **Observations ride the wire type.** An io observation is the one display
  content the protocol records cannot supply; it crosses as its total
  `FOValue` wire form and the view fold rebuilds the card through the same
  mark builders the live path uses — a mark tree is a rendering, never a fact.
- **Consolidations bought on the way.** A `Display::Result` names its call by
  `BlockId` instead of a nearest-resident tail walk; the three
  `emit_context_edited` call sites collapse into one authority,
  `AgentLog::apply_edit`, whose published record is the one notification; and
  usage meters at the seam's sink, so a display-muted child still counts.

## What resume now does

`tui_loop::run` folds `record.jsonl` into `Blocks` via `record::replay`
*before the worker spawns*, seeds the viewport with one `Viewport::sync`
call, and restores cumulative usage from the replayed `UsageDelta` rows —
the "resumed" note is the boundary between replayed history and the live
session. The founding bug has a test (`exarch/tests/resume.rs` asserts the
view fold's output across a kill and resume). A session recorded before
this change has no `record.jsonl` and is **refused with a named error**
rather than silently started empty; no migrator is written. `user.log`
becomes a regenerable render of the fold's resident blocks, written whole at
flush points and never patched — which also removes the old incremental
tee's data-loss bug, where a truncate-and-rebuild past eviction silently
deleted the session's own evicted transcript from disk.

## What is deliberately not unified: live display

**All four blockers below are since closed**: the channel carries `Signal
{ Fact, Transient }` alone, and every frontend folds it.  The reasoning stands
recorded because each blocker was a real design question, not an oversight.

The plan's end-state — both printers driven live by `Printer::sync(&Blocks)`
over a live-folded stream — **did not land, on purpose**. The live dispatch
loop (`tui_loop::ui_loop` → `App::handle`) still runs entirely on the
`Kind`-tagged stream. The channel carries `Signal { Event, Fact, Transient }`;
`Signal::into_event` is the *transitional bridge* that projects a seam fact
back to a legacy `Kind` for exactly the seven retired twins whose dual-write
emit sites the seam collapsed (`Step`, `ContextEdited`, `Usage`, `Error`,
`Nudge`, `ProviderError`, `Stalled`) — every other class keeps a live legacy
emit beside its record, so deriving a `Kind` there too would draw it twice.
`Viewport` and `Headless` implement `Printer`, exercised by tests and by
resume seeding only. Four real blockers, each wanting its own design
decision rather than a unilateral call mid-implementation:

1. **Live chrome has no interleaving mechanism.** The banner, `/help` and
   `/copy` acks, `/resources` rows, and stop-reason lines are deliberately
   never recorded ("drawn, not recorded"), while `sync` rebuilds resident
   blocks wholesale from the fold — mixing the two on one live viewport
   erases the chrome. A designed merge of folded blocks and unrecorded rows
   does not exist yet.
2. **UI-authored facts have vocabulary but no door.**
   `Forensic::SystemNote` and `Forensic::ModelChanged` exist in the record
   vocabulary with no production emit site: the UI thread has no
   `record::Emitter` plumbed to it. This is the seam-side half of 260623's
   "the screen never invents events" promise, still unbuilt — a model switch
   records in the transcript today, not in the one log, so the
   context-floor denominator `ModelChanged` was minted for is not yet fed.
3. **Some errors cannot record themselves by construction.** The
   worker-panic report in `bus/sink.rs` and the seam's own append-failure
   reporters emit `Kind::Error` with no record behind it; a fold-only live
   view would silently lose exactly the failures it most needs to show.
4. **`ContextEdited`'s display row is bridge-only.** Live, the row rides
   `record_kind`'s projection of the protocol record; the view fold skips
   `Protocol` entirely, so a resumed scrollback has no context-edit row. The
   plan's own disposition assigns it "a notice commit from the producer";
   `NoticeFact` has no variant for it — a small frozen-surface gap left open
   rather than inventing vocabulary mid-implementation.

`transcript.jsonl` is likewise **not** retired to a filtered projection
(the plan's step 8): it remains an independently written trace, fed at the
bus emit seam outside these parcels' boundary, and says so in its own
header. The 260623 debt is therefore paid for the durable log and for
resume, and open for live display and the transcript.

**Superseded**: `dev/docs/plans/260814_kind_dissolves.md` deletes
`transcript.jsonl` outright rather than folding it — its three genuinely
unique facts move elsewhere (`Entry.at_unix_ms` per record, a child's own
`SessionStarted`/`SessionEnded` for `born`/`died`, `stop_reason` already on
`Protocol::AssistantMessage`), and the fourth, `/resources`' figures, is
named a loss rather than answered: no session keeps a pressure history, so
`invariants/probe-convention.md`'s "so `transcript.jsonl` keeps the figures"
clause is amended, not carried forward. `record.jsonl` is now the one durable
log with no independent sibling.

## Accepted losses

- Committed text is stored more than once: assistant markdown verbatim in
  the protocol record and again chopped in its display commits; a tool
  result in up to three shapes, byte-identical by the standing law that the
  user never sees more of a result than the model did. Deriving display from
  protocol would need two choppers proven confluent and would re-couple the
  folds this decision exists to separate.
- Log growth rises as observations move in; 260812's unbounded-growth
  acceptance and deferred segment rotation carry over, with `/clear` as the
  rotation boundary. Resume still reads the whole ledger — the same O(file)
  bill, now for space as well as time.
- "Recorded" means surrendered to the OS: per-record flush, never `fsync` —
  process-crash durable, not power-loss durable; the quarantined tail covers
  the torn write either way.
- A pre-plan session does not resume; `--no-logs` has a fileless seam that
  still stamps and publishes, and no resumable history, unchanged.
- Dial state and the provisional thinking seat are UI state, not records,
  and do not survive a resume.

## See also

[[decisions/260623_recording-follows-the-event|recording-follows-the-event]]
(the debt this pays, half), [[decisions/260812_context-is-a-projection|context-is-a-projection]]
(the fold law this generalises from the model view to the log),
[[map/exarch/frontend|frontend]] (the as-built arm),
[[design/residency|residency]] (the viewport's accumulator/memo split).
