---
verified_at_commit: d9abfb52
verified_at_date: 2026-09-11
anchors: [Emitter::emit, Log::append, Log::read, Signal::Fact, Signal::Transient, Record, Protocol, Display, Forensic, Transient, Model::step, View::step, BLOCKS_WINDOW, Blocks::step, Delta, Sink::fact, Blocks::model, replay, model::resume, Scrollback::fact, Scrollback::trim, seed, flush_log, rotate, clear, Context, Turn, Body, Pointer, TurnRow, Held, Locus, render_marker, Context::step, Context::plan_eviction, Context::resolve_cut, Context::place]
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
turn. A log keeps what it appended while it had no sink at all — a session's
head bookend, a fork's inherited context — and `attach` publishes that backlog
in order through the arriving sink, so the seam delivers every record to the
sink exactly once, whenever the sink arrives. Redelivery is safe because the
view fold ignores a `Seq` it has already folded: a resumed memo seeded from
the file is not stepped a second time by the same head records.

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
  tool calls and results, one observation per observation, cards, notices, done
  outcomes, and other committed rows. `record/commit.rs` does the chopping
  before the fact reaches the seam; grouping a call's effects is the frontend's,
  derived online ([[map/exarch/io-surface|io-surface]]). Cards and observations
  carry their round-trippable data, not a pre-rendered terminal image; the view
  fold builds the marks again.
- `Forensic` is durable evidence that is not model context: the session
  bookends (`SessionStarted`, `SessionResumed`, `SessionEnded`), the shape a
  turn's request goes out under — sampling and routing both (`TurnStarted`),
  usage deltas, cancellation,
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

- `turns: Vec<Turn>` in id order — `{ id, role, kind, label, bytes, body }` —
  `role` being `user` or `assistant`, and `kind` `own`, `import` or
  `inherited`; which assistant turns answer a given prompt is a function of
  role and order, derived where the survivor rule and `plan_eviction` need it
  and held nowhere
  ([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]);
- a turn's `body` is either `Here { records, origin }`, its records in memory,
  or `There { at, cut }`, a `Pointer` to the file and byte ranges that hold
  them; the *context* is the `Here` subsequence;
- `notes: Vec<Option<String>>`, one slot per eviction, which `There::cut`
  indexes — `None` is a cut made without a note, the user's rewind and the
  harness's own, and the marker then has no line to draw;
- `source`, this log's own `record.jsonl`, where a turn recorded here points
  once it leaves; the protocol's resting state; and the count of records
  folded, beside the index of the newest context edit.

Those first three live in a `Table` private to `model/table.rs`, which hands
the rest of the module a `&[Turn]` and nothing writable. A turn therefore
changes where it is only through `Table::evict`, the one function that moves a
turn from here to there — so the invariant *the
first resident turn is a user turn* is kept by construction rather than by
every caller remembering the survivor rule.

`Turn` and `Body` are private to the module: a turn holds records, so it is
not a value to hand out. Two projections take its place. `Turn::row` yields a
`TurnRow { id, role, kind, label, bytes, held }`, `Held` read *off the body* —
`Here` is `Resident`, `There { cut }` is `Evicted { cut }`, and there is no
third state — so residency and the structure cannot
disagree, and a row states which eviction took its turn rather than leaving a
reader to infer it from an id window. `Context::linked` yields one
`Linked { row, at }` per turn: the row, and the address of its transcript
copy. Those two, with `TurnRow`, `Held`, `Pointer` and `Linked`, are the whole
public surface; `Display::Context` carries `Vec<TurnRow>`.

Everything the model or the human reads off the structure is a pure function
of it, computed on call and memoised nowhere: `rendered`, `history_bytes`,
`context_survey`, `transcript_index`, `render_marker`, `plan_eviction` — which
answers the whole set a pressure cut would take — and `sourced`.
`Context::step` is the one
fold — it judges a record before applying it, so a hand-edited or foreign file
is refused by the same function that folds a live one, and a resume has
nothing to compare its result with. `Table::evict`
is the one place a turn moves from here to there.

**A recorded cut is the resolved cut.** `Context::resolve_cut` applies the
survivor rule at the writer — a prompt whose answers still hold a resident
turn outside the set stays — so `Cut { turns, note }` names the ids
that actually left. Replay departs exactly those and `judge` refuses a record
naming a turn that is not resident, re-deriving nothing
([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]).

**Eviction is a change of address, not a deletion.** The table sets each
departing turn's body to `There { at, cut }`, where `at` is its `origin`
if it has one and otherwise this log's `source` beside the `Locus` of every
record it held; the records leave memory and nothing durable is touched. Every
read of a range checks the record's bytes against the digest the `Locus`
carries, *before* the parse, so a range that no longer names what it
measured — a rotated segment, a copied session directory, an edited log — is
refused as the mismatch it is rather than answered with whatever now lies at
those offsets. The door reads a turn at a time, so a search never holds more
than one turn, and it holds one descriptor, reopening only where
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

`Context::rendered()` builds an owned `Vec<ChatMessage>`, with no render memo
behind it. Assembly is one walk: at each **hole** — a maximal run of departed
turns — the one marker `render_marker` yields for it, and every resident turn's
messages, in order — a prompt whose answers were interrupted
short of a reply included, as they lie
([[invariants/turn-ends-ready|exchange-ends-ready]]). `history_bytes` is the
same walk over the per-turn sums with no turn rendered at all, and
`context_survey` reports it as `total-bytes` beside the rows rather than
summing them.

`render_marker` renders on every call too. Per hole it draws one role-marked
row per departed turn, grouped by the cut their body names, with `notes[i]`
after cut *i*'s rows, in one bracketed user-voice message standing at the
hole's own position — `[EXARCH // Turns 1–29, 31–35 have left your context. …]`
over rows like `  41  assistant  <label>  12 KB`, forty rows to a hole before
the rest collapse into a line naming their range. The hole
at position 0 is the marker a prefix cut leaves; a later cut touches its own
hole and nothing else, so the provider re-reads from the hole rather than from
the start. Correct by
construction, since the body says which cut took a turn; and a message stays
byte-stable between edits not because it is cached but because it is a
function of a structure that did not change. This is the model fold's
recompute invariant taken to its end: there is no memo to read as authority,
and nothing to keep in step.

The price is one build of the context per provider request — once per turn,
never per loop iteration — on top of the clone per HTTP attempt at the wire
door, which is where
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] put
it and where it stays: `manufacture` takes `&[ChatMessage]` inside
`retry_with_backoff`'s per-attempt closure.
[[map/exarch/provider|the provider map]] describes that one door,
`provider/wire.rs`, where those messages become an owned `genai::ChatRequest`.

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

The view path is `Blocks::step` over `Display` and `Forensic`; it skips
`Protocol` explicitly. `Blocks::push` joins consecutive records of one lane
into a block, while a different kind opens the next block. `Block` construction
is private to the fold, and the memo keeps a bounded resident window
(`BLOCKS_WINDOW`) — the one window, for every frontend.

Each step reports what it did as a `Delta`: `Opened` a block, `Grew` the lane
the tail held, `Patched` a call with its result, or `Quiet`. A frontend is a
`bus::Sink`: it owns its own `Blocks` memo, steps it through
`Sink::fact(id, rec)` — the witnessed record, since a fold is stepped by the
record *and* its locus — and draws that increment, so a frontend is itself a
fold over the one log, and cannot invent a third block projection: it renders
from `BlockKind` off its memo, never from the record vocabulary
([[decisions/260909_the-fold-reports-the-printer-mirrors|the-fold-reports-the-printer-mirrors]]).
Beside the blocks the memo holds the ambient facts none of them draws:
cumulative usage from the forensic deltas, and the model in force — named by
the session's head bookend, renamed by each `Forensic::ModelChanged`, so
`Blocks::model` is total from a session's first record and the context floor a
frontend stamps prose with reads both of its terms off the fold rather than
being told the denominator.
The TUI keeps a 1:1 mirror of its memo, one `tui::Block` per incident, with
deliberation and the work it ordered collapsed into a group as they land;
headless keeps one memo per source agent and prints each opened block.

Live and replay use the same view fold. The TUI's `App::fact` and
`App::transient` are the two distinct doors: durable blocks are fold-backed,
while an open answer/thinking line or chrome row remains provisional until a
later record or boundary resolves it.

## Resume and the user view

On TUI resume, `tui_loop` replays `record.jsonl` into `Blocks` before the
worker starts and hands that memo to the scrollback, which becomes its owner:
`Scrollback::seed` builds its mirror block by block, exactly as a live commit
does, and marks it as already present in `user.log`. The resumed session
appends after that seeded prefix instead of writing the replayed window twice.
Cumulative usage comes from the replayed forensic deltas; the resumed note is
the boundary between history and new live signals.

`user.log` is the rendered user view, not the source of truth. There is one
window: after every step `Scrollback::trim` walks the mirror's head against the
fold's own first block, renders what it drops once into the retired prefix, and
advances the prefix's durable offset. Resident
blocks are provisional: `Scrollback::flush_log` writes them past that prefix for
session-end output and `/export`, while the next retirement rewinds to the
prefix before extending it, so no block is duplicated. A tombstoned scrollback
retires its remaining blocks before dropping its heap state; there is no
reload-from-`user.log` fold. Crash recovery remains the responsibility of
`record.jsonl`, which is flushed per record.

## Clear and segment rotation

`/clear` cancels the in-flight exchange, resets the scrollback (including its
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
and `user.log` is the scrollback's rendered stream: a retired prefix plus a
provisional resident tail.

See [[decisions/260814_one-seam-one-log|one-seam-one-log]] for the seam and
fold law, [[decisions/260814_a-trace-is-a-fold|a-trace-is-a-fold]] for the
single durable record, [[decisions/260816_the-window-is-not-the-transcript|the-window-is-not-the-transcript]]
for retirement, and
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] for
the live bus lifetime. The broader accumulator/fold distinction is in
[[design/residency|residency]], and the visual projection discipline is in
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]].
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] is the
ADR for the one wire door, and
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] for the
structure every view above projects — the turn as the atom of eviction, and
the turn's own body as the one statement of where it is, and
[[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]
for the one edit that moves a turn and the marker each hole carries;
[[map/exarch/provider|provider]] covers the one door that turns those messages into an owned wire
request.
