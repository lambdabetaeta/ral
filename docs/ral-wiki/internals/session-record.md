---
verified_at_commit: cbeb5457
verified_at_date: 2026-08-17
anchors: [Emitter::emit, Log::append, Log::read, Signal::Fact, Signal::Transient, Record, Protocol, Display, Forensic, Transient, Model::step, View::step, BLOCKS_WINDOW, Printer::sync, replay, model::resume, Viewport::commit_fact, seed, enforce_window_caps, flush_log, rotate, clear]
---

# Session record: one seam, one log

**A session has one durable source: `sessions/<id>/record.jsonl`.** Workers
author facts through one cloneable `record::Emitter`; the model projection, the
scrollback projection, and the rendered `user.log` are folds or presentations
of that record. The bus is delivery, not a second authority. This is the
operational path behind the [[map/exarch/frontend|frontend]],
[[map/exarch/cards|cards]], and [[map/exarch/io-surface|io-surface]] maps.

## The seam

`Emitter::emit` is generic over the sealed `record::Class` set, so a live
producer can append only a `Protocol`, `Display`, or `Forensic` record. It
delegates to `record::Log::append`, whose mutex protects the whole critical
section:

1. Wrap the record with its append timestamp and serialize the `Entry` envelope.
2. Write and flush the line when the session is durable; `--no-logs` keeps the
   same seam with no writer.
3. Advance the `Seq` and byte cursor and build the `Recorded<Record>` stamp.
4. Meter usage where applicable and publish `Signal::Fact(AgentId, recorded)`
   through the attached weak fleet sink before releasing the lock.

Append-then-publish makes channel order log order. In a durable session, a
missing or slow receiver cannot lose the fact: it is already in the file and a
later replay can catch up. The sink is attachable because the session log
outlives a TUI session bus and the headless per-exchange buses that are attached
to it in turn.

`Emitter::transient` uses the same log mutex for ordering but never writes a
line or takes a sequence number. It publishes the other channel passenger,
`Signal::Transient(AgentId, Transient)`. A `Signal::Fact` is the recorded,
stamped half (file-backed in durable sessions); a `Signal::Transient` is
live-only and has no replay path.

## Three durable classes, one live edge

The outer `Record` vocabulary is closed:

- `Protocol` is the provider-facing history: session bookends, prompts,
  context messages, step starts, assistant messages, tool results, and context
  edits. The model fold consumes this class alone.
- `Display` is worker-authored presentation data: chopped prose and reasoning,
  tool calls and results, grouped observations, cards, notices, done outcomes,
  and other committed rows. `record/commit.rs` does the chopping and grouping
  before the fact reaches the seam. Cards and observations carry their
  round-trippable data, not a pre-rendered terminal image; the view fold builds
  the marks again.
- `Forensic` is durable evidence that is not model context: usage deltas,
  cancellation, provider/stall/error rows, nudges, and other breadcrumbs. The
  view fold admits the rows that belong on scrollback; the model fold ignores
  them.

`Transient` is deliberately outside `Record`: token and thinking deltas, state
changes, boundaries, child lifecycle, stop reasons, clear acknowledgements,
live pins/resources, and seam faults. These are drawn or routed while the
process is alive. They are not a hidden fourth record class, and no resume
attempts to reconstruct them.

## The two folds

`record::Fold` gives both projections one driver. `record::replay` streams
`record::Log::read` one line at a time, reconstructs each `Recorded<Record>`,
and calls the fold's exhaustive `step`; a parse error or an unrecognised
record is a `Refusal`, so replay does not silently skip a vocabulary change.

The model path is `record::Model::step` over `Protocol` records only. During a
live turn, `AgentLog::advance` applies that same step immediately after the
seam returns from `emit`. Its `Memo` owns the protocol state, exchange view,
and ledger. When context edits evict old protocol records, the ledger keeps
their `Stamp` byte ranges and reads those lines back from `record.jsonl` when a
refold needs them; no recorded protocol fact is deleted and the whole log is
never held in memory.

`record::model::resume` quarantines a torn crash tail, then streams the file
through admission and the model fold, checking the incrementally maintained
projection against a refold. A missing `record.jsonl` is a named refusal, not
an invitation to start an empty resumed session.

The view path is `record::View::step` over `Display` and `Forensic`; it skips
`Protocol` explicitly. `Blocks::push` joins consecutive records of one lane
into a block, while a different kind opens the next block. `Block` construction
is private to the fold, and the memo keeps a bounded resident window
(`BLOCKS_WINDOW`). A `record::Printer` receives `Blocks`, not raw records, so
the TUI and headless printers cannot invent a third block projection.

Live and replay use the same view fold. `Signal::Fact` reaches
`Viewport::commit_fact` or headless absorption, which steps the memo and calls
`Printer::sync`; `Signal::Transient` goes straight to the printer's live edge.
The TUI's `App::fact` and `App::transient` are therefore the two distinct
doors: durable rows are fold-backed, while an open answer/thinking line or
chrome row remains provisional until a later record or boundary resolves it.

## Resume and the user view

On TUI resume, `tui_loop` replays `record.jsonl` into `Blocks` before the worker
starts, then `Viewport::seed` performs one sync and marks the resident rows as
already present in `user.log`. The resumed session appends after that seeded
prefix instead of writing the replayed window twice. Cumulative usage comes
from the replayed forensic deltas; the resumed note is the boundary between
history and new live signals.

`user.log` is the rendered user view, not the source of truth. The fold memo is
bounded independently from the viewport's presentational caps. When
`Viewport::enforce_window_caps` evicts the oldest blocks, it renders them once
into the retired prefix and advances the prefix's durable offset. Resident
blocks are provisional: `Viewport::flush_log` writes them past that prefix for
session-end output and `/export`, while the next retirement rewinds to the
prefix before extending it, so no block is duplicated. A tombstoned viewport
retires its remaining blocks before dropping its heap state; there is no
reload-from-`user.log` fold. Crash recovery remains the responsibility of
`record.jsonl`, which is flushed per record.

## Clear and segment rotation

`/clear` cancels the in-flight exchange, resets the viewport (including its
`user.log` segment), and arms the frontend's drain gate so straggler signals
from the old exchange cannot paint the new context. The `Cleared` transient, or
the next fresh prompt when that acknowledgement is lost, closes that gate.

The session record is rotated without replacing the seam. `AgentLog::clear`
renames the current segment, then `Emitter::rotate` asks the same shared
`record::Log` to open a fresh `record.jsonl` and reset its sequence/cursor while
retaining the attached `FleetSink`. Existing `Emitter` clones and the bus
coupled before the clear therefore continue publishing into the new segment.
The `--no-logs` branch rotates to the same mirror-only seam with no writer.

The resulting trust boundary is small: `record.jsonl` is the durable fact
stream, `Record` classes say which fold may project each fact, `Signal::Fact`
delivers stamped commits live, `Transient` carries only process-lifetime edges,
and `user.log` is the viewport's rendered stream: a retired prefix plus a
provisional resident tail.

See [[decisions/260814_one-seam-one-log|one-seam-one-log]] for the seam and
fold law, [[decisions/260814_a-trace-is-a-fold|a-trace-is-a-fold]] for the
single durable record, [[decisions/260816_the-window-is-not-the-transcript|the-window-is-not-the-transcript]]
for retirement and incremental sync, and
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] for
the live bus lifetime. The broader accumulator/fold distinction is in
[[design/residency|residency]], and the visual projection discipline is in
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]].
