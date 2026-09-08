---
verified_at_commit: 1d028de7
verified_at_date: 2026-09-08
anchors: [Emitter::emit, Log::append, Log::read, Signal::Fact, Signal::Transient, Record, Protocol, Display, Forensic, Transient, Model::step, View::step, BLOCKS_WINDOW, Printer::sync, replay, model::resume, Viewport::commit_fact, seed, enforce_window_caps, flush_log, rotate, clear, Context, Turn, Body, Pointer, Row, Held, Locus, Rendered, render_head, Context::step, Context::plan_eviction, apply_context_op, Context::place]
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
3. Advance the `Seq` and byte cursor and build the `Recorded<Record>` locus.
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

- `Protocol` is the provider-facing history: prompts, context messages, the
  ancestry link, assistant messages, tool results, and context edits. The
  model fold consumes this class alone, and every record in it acts on the
  fold.
- `Display` is worker-authored presentation data: chopped prose and reasoning,
  tool calls and results, grouped observations, cards, notices, done outcomes,
  and other committed rows. `record/commit.rs` does the chopping and grouping
  before the fact reaches the seam. Cards and observations carry their
  round-trippable data, not a pre-rendered terminal image; the view fold builds
  the marks again.
- `Forensic` is durable evidence that is not model context: the session
  bookends (`SessionStarted`, `SessionResumed`, `SessionEnded`), a turn's
  effort dial (`TurnStarted`), usage deltas, cancellation,
  provider/stall/error rows, nudges, and other breadcrumbs. The view fold
  admits the rows that belong on scrollback; the model fold ignores them.

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

The log is the durable structure; `Context` is its fold in memory — the
vocabulary in `record/model.rs`, the turns and cuts in `model/table.rs`, the
automaton in `model/state.rs`, the fold in `model/fold.rs`, the render in
`model/render.rs`, the transcript door in `model/transcript.rs`, resume in
`model/resume.rs` — and it holds **every turn the lineage has recorded, and
where each one is**
([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]):

- `turns: Vec<Turn>` in id order — `{ id, exchange, kind, label, bytes, body }`
  — where a user turn is the one with `id == exchange`;
- a turn's `body` is either `Here { records, origin }`, its records in memory,
  or `There { at, cut }`, a `Pointer` to the file and byte ranges that hold
  them; the *context* is the `Here` subsequence;
- `notes: Vec<Option<String>>`, one slot per eviction, which `There::cut`
  indexes — `None` there is a drop, which the marker does not list;
- `source`, this log's own `record.jsonl`, where a turn recorded here points
  once it leaves; the protocol's resting state; and the count of records
  folded, beside the index of the newest context edit.

Those first three live in a `Table` private to `model/table.rs`, which hands
the rest of the module a `&[Turn]` and nothing writable. A turn therefore
changes where it is only through `Table::evict` or `Table::drop_exchanges`,
the two functions that know which turns a cut takes — so the invariant *the
first resident turn is a user turn* is kept by construction rather than by
every caller remembering the survivor rule.

`Turn` and `Body` are private to the module: a turn holds records, so it is
not a value to hand out. Two projections take its place. `Turn::row` yields a
`Row { id, exchange, kind, label, bytes, held }`, `Held` read *off the body* —
`Here` is `Resident`, `There { cut: Some(c) }` is `Evicted { cut: c }`,
`There { cut: None }` is `Dropped` — so residency and the structure cannot
disagree, and a row states which eviction took its turn rather than leaving a
reader to infer it from an id window. `Context::linked` yields one
`Linked { row, at }` per turn: the row, and the address of its transcript
copy. Those two, with `Row`, `Held`, `Pointer` and `Linked`, are the whole
public surface; `Display::Context` carries `Vec<Row>`.

Everything the model or the human reads off the structure is a pure function
of it, computed on call and memoised nowhere: `rendered`, `history_bytes`,
`context_survey`, `transcript_index`, `render_head`, `plan_eviction`,
`sourced`, and the pressure reminder's `through`. `Context::step` is the one
fold — it judges a record before applying it, so a hand-edited or foreign file
is refused by the same function that folds a live one, and a resume has
nothing to compare its result with. `apply_context_op` is the one place a turn
moves from here to there.

**Eviction is a change of address, not a deletion.** `apply_context_op` sets
each departing turn's body to `There { at, cut }`, where `at` is its `origin`
if it has one and otherwise this log's `source` beside the `Locus` of every
record it held; the records leave memory and nothing durable is touched. Every
read of a range checks the record's bytes against the digest the `Locus`
carries, *before* the parse, so a range that no longer names what it
measured — a rotated segment, a copied session directory, an edited log — is
refused as the mismatch it is rather than answered with whatever now lies at
those offsets. The door reads a turn at a time, so a search never holds more
than one exchange, and it holds one descriptor, reopening only where
consecutive turns name different logs. The log is thereby the model's
transcript as well as its identity
([[decisions/260906_context-rollover|context-rollover]]).

**A turn's transcript copy is where it was first recorded.** A fork's link
(`Protocol::Inherited { turns: Vec<Linked>, notes }`) carries an address for
every row, resident or not, so a turn a `mnemon` seed re-recorded only in part
still reads back whole from its `origin`, and evicting it in the child points
at that original copy rather than the short one. The lineage is flattened at
each fork — a grandchild's row already names the grandparent's file — so
nothing walks an ancestry, and `Context::sourced` is the whole of "seek in the
structure, then the file".

### The provider-facing context is a pure function of the structure

`Context::rendered()` builds a `Rendered { messages: Vec<ChatMessage>, bytes }`
— owned, with no render memo behind it. Assembly is one walk: the head marker
where `render_head` yields one, then the exchanges holding a resident turn, in
order. Each earlier exchange renders as its turns' messages if it is settled
and as its one `abandoned_note` if not; the last is the exchange *in hand* and
renders as it lies, since its last turn may still be growing and an abandoned
exchange is not yet abandoned while it is the one in hand. Settled is a look
at one record — the last of that exchange's last resident turn — a turn's body
holding material alone. `history_bytes` is the same walk over the per-turn
sums with no turn rendered at all, and `context_survey` reports it as
`total-bytes` beside the rows rather than summing them.

`render_head` renders on every call too. It groups the departed rows by the
cut their body names — maximal runs of table-adjacent rows sharing an exchange
and a cut — and draws `notes[i]` after cut *i*'s rows. Correct by
construction, since the body says which cut took a turn; and message 0 stays
byte-stable between edits not because it is cached but because it is a
function of a structure that did not change. This is the model fold's
recompute invariant taken to its end: there is no memo to read as authority,
and nothing to keep in step. Rendering the marker on every call also closes a
defect the cached one had, where a drop that emptied a partially evicted
exchange left the sentence describing turns that had since left whole.

The price is one build of the context per provider request — once per turn,
never per loop iteration — on top of the clone per HTTP attempt at the wire
door, which is where
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] put
it and where it stays: `manufacture` takes `&Rendered` inside
`retry_with_backoff`'s per-attempt closure.
[[map/exarch/provider|the provider map]] describes that one door,
`provider/wire.rs`, where a `Rendered` becomes an owned
`genai::ChatRequest`.

The other place an owned whole-history `Vec<ChatMessage>` is materialised is
`Context::inherited_seed` — one entry per resident parent turn under the
parent's own ids, its last turn cut to `admissible_prefix`, the longest run
owing no tool result — the context a `mnemon` child inherits at spawn, where
ownership genuinely transfers into the child's own log. The link's rows and
notes cross beside it, so the child's marker, survey and index are the same
projections of the same structure rather than values copied over.

`record::model::resume` quarantines a torn crash tail, reads the session's
identity off the file's first record, and streams every `Protocol` record
through `Context::step`. There is no second fold and nothing to compare: what
guards a read-back is the digest, at the moment of the read, where the risk
is. A missing `record.jsonl` is a named refusal, not an invitation to start an
empty resumed session.

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
ADR for the one wire door, and
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] for the
structure every view above projects — the turn as the atom of eviction, and
the turn's own body as the one statement of where it is;
[[map/exarch/provider|provider]] covers the one door that turns a `Rendered`
into an owned wire request.
