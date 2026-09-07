---
verified_at_commit: 4bc5006c
verified_at_date: 2026-09-07
anchors: [Emitter::emit, Log::append, Log::read, Signal::Fact, Signal::Transient, Record, Protocol, Display, Forensic, Transient, Model::step, View::step, BLOCKS_WINDOW, Printer::sync, replay, model::resume, Viewport::commit_fact, seed, enforce_window_caps, flush_log, rotate, clear, Folded, Turn, Cut, Held, Rendered, TurnRender, render_head, Memo::plan_eviction, apply_context_op, record_turn]
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
2. Write and flush the line.
3. Advance the `Seq` and byte cursor and build the `Recorded<Record>` stamp.
4. Meter usage where applicable and publish `Signal::Fact(AgentId, recorded)`
   through the attached weak fleet sink before releasing the lock.

Append-then-publish makes channel order log order, so a missing or slow
receiver cannot lose the fact: it is already in the file and a later replay
can catch up. The sink is attachable because the session log outlives a TUI
session bus and the headless per-exchange buses that are attached to it in
turn.

`Emitter::transient` uses the same log mutex for ordering but never writes a
line or takes a sequence number. It publishes the other channel passenger,
`Signal::Transient(AgentId, Transient)`. A `Signal::Fact` is the recorded,
stamped, file-backed half; a `Signal::Transient` is live-only and has no
replay path.

## Three durable classes, one live edge

The outer `Record` vocabulary is closed:

- `Protocol` is the provider-facing history: session bookends, prompts,
  context messages, the ancestry link, turn starts, assistant messages, tool
  results, and context edits. The model fold consumes this class alone.
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
seam returns from `emit`.

### One structure, one fold, everything else a projection

The log is the structure; `Folded` is its fold
([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]):

- a **table of `Turn`s** in id order — `{ id, exchange, kind, label, bytes,
  held }`, every turn the lineage recorded, resident or departed — where the
  *context* is the `Held::Resident` subsequence and a user turn is the one
  with `id == exchange`;
- the **`Cut`s** made in it, each `{ through, note }`, one per eviction;
- the protocol's resting state.

Everything the model or the human reads off it is a pure function of that
fold: the context sent to the provider, `context_survey`, `transcript_index`,
`render_head`, `plan_eviction`, and the pressure reminder's `through`.
`apply_context_op` is the one place a turn's `held` changes, shared by the live
step and the refold, so a resume cannot disagree with the session it replays.

**Placement lives in the ledger, not on the turn.** `Ledger::placement`
carries, per slot, the id of the turn that slot belongs to; runs are
contiguous and ids monotone, so `Ledger::events(turn)` is two
`partition_point`s. A `Turn` therefore carries no range, and the same table
serves a lineage whose older turns lie in an ancestor's file — their placement
is foreign, held in the lazily walked `ancestry_index`.

When a context edit takes a turn out, the ledger frees that turn's records and
keeps their `Stamp` byte ranges, reading those lines back from `record.jsonl`
when a refold needs them; no recorded protocol fact is deleted and the whole
log is never held in memory. Every read of a range checks the record's bytes
against the digest the `Stamp` carries, before the parse, so a range that no
longer names what it measured — a rotated segment, a copied session directory,
an edited log — is refused as the mismatch it is rather than answered with
whatever now lies at those offsets. The refold is not the only reader:
`transcript` reads those same ranges back on demand, decoding turn by turn so
that a search never holds more than one exchange. The log is thereby the
model's transcript as well as its identity
([[decisions/260906_context-rollover|context-rollover]]).

### The provider-facing context is a persistent value

`Memo::rendered()` does not walk the ledger and materialise owned
`genai::ChatMessage`s on every call. It returns a `Rendered`
(`record/model.rs`): `Vec<Arc<[ChatMessage]>>` segments plus a byte
total summed from the table's own per-turn weights, private fields, clone is
`Arc` bumps. The committed history is immutable and append-only — one turn
adds one assistant message and its tool results, and nothing already recorded
ever changes — so the memo caches each turn's rendering, keyed by turn id
beside that turn's own end slot (`TurnRender { end, segment }`). A turn id
never recurs, so `(id, end)` determines a rendering globally and forever, and
a stale cache entry is inexpressible: a still-growing turn simply misses the
key it would need to hit. `render_turn` takes no flags, so nothing can vary
one — the memo's key is the whole of the function's input.

Assembly is then one walk: the head marker segment (recomputed by
`recompute_head` whenever the table or its cuts change, present exactly when
`render_head` says so — the one notion of "no marker" there is), then each
resident exchange. A closed exchange whose own fold does not settle reads as
its one `abandoned_note`; otherwise its turns' cached renders in order. The
exchange *in hand* renders as it lies, since its last turn may still be
growing and an abandoned exchange is not yet abandoned while it is the one in
hand. `history_bytes` is that assembly's own byte total, and `context_survey`
reports it as `total-bytes` beside the rows rather than summing them. This
preserves the model
fold's recompute invariant rather than contradicting it: correctness never
reads the memo as authority, it is a memo of a pure function at immutable
arguments — droppable and reconstructible at any moment, never serialised,
rebuilt from nothing by this fold on resume. "Recomputed on every call"
becomes cheap instead of false.

The one remaining place an owned whole-history `Vec<ChatMessage>` is
materialised outside the wire is `Memo::inherited_seed` — one entry per
resident parent turn under the parent's own ids, its last turn cut to
`admissible_prefix`, the longest run owing no tool result — for the context a
`mnemon` child inherits at
spawn, where ownership genuinely transfers into the child's own ledger. The
parent's whole table and its cuts cross beside it, on the
`Protocol::Inherited` link, so the child's marker is the same projection of
the same fold rather than a value copied over. Every
other crossing carries a `Rendered` by shared reference, the provider seam
included;
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] is
the ADR, and [[map/exarch/provider|the provider map]] describes the one door,
`provider/wire.rs`, where a `Rendered` is finally turned into an owned
`genai::ChatRequest`.

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
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] is the
ADR for the persistent value and the per-turn render cache
described above, and
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] for the table
it projects; [[map/exarch/provider|provider]] covers the one door that
turns it into an owned wire request.
