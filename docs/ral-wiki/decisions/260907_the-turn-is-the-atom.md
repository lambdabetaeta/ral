---
status: 'active; amended in carrier — the same decision carried through to residency: one `Context` holding every turn and *where* it is, `Held` a projection off it. See *Amended: the context is one structure*.'
generated_at_commit: 14c8cae4
---

# The turn is the atom of eviction

**The context is a table of *turns*; an eviction is a cut in it, addressed by
a turn id, weighed at every turn boundary.** One growing structure, one fold,
everything else a projection: the log is the structure, `Folded` — a table of
`Turn`s and the `Cut`s made in it — is its fold, and the context sent to the
provider, the survey, the transcript index, the head marker, the pressure
reminder and the eviction plan are all pure functions of that table.

## Why

An agentic model takes one prompt and then two hundred tool turns. A rollover
that evicts whole *exchanges*, plans only over closed ones, structurally
cannot name the newest, and is weighed once at `deliberate` entry has, in that
run, exactly one exchange — the one nobody may name — weighed at a boundary
that never comes. The provider refuses the request, the deliberation dies, and
the *next* prompt's entry eviction sheds the whole giant exchange to a
50-character row: the task evaporates precisely when it was in hand.

The protocol is well-formed at every **turn boundary** — an assistant message
and the tool results answering it — not only at exchange boundaries.
`can_evict` already knew this (`admits_new_turn` holds in
`AwaitingAssistantAfterToolResults`); only the plan's shape and the call site
pretended otherwise.

## Vocabulary

- **Context** — what the model is sent. Never "window" or "view"; *context
  window* survives only as the provider's capacity figure.
- **Transcript** — the record the door reads: `record.jsonl` and the
  ancestry's. Never "store".
- **Turn** — the atom. A *user turn* is a prompt, or an import's opening, and
  anything before the first reply; an *assistant turn* is the assistant
  message, the tool results it called for, and any steering before the next
  request. Every id-bearing record opens one; everything else extends the last
  (`record_turn`).
- **Exchange** — the run of turns beginning at a user turn; its id is that
  turn's, so a user turn is `id == exchange` (`Turn::is_user`).
- **Cut** — one eviction: `through`, and the model's optional note.
- **No "step".** The round-trip is the act of taking a turn: `TurnStarted`
  carries its meta half (the effort dial), `Display::Turn { id }` the id the
  request will produce, `MAX_TURNS` (250) is the loop's local ceiling. A
  cancelled request retaken shows the same id twice — accurately.

## The decisions

1. **One id space.** User and assistant turns take ids from one
   lineage-monotone sequence, so exchange numbers go sparse (1, 5, 12, …) —
   intended. The id lives on `UserPrompt { exchange }` and
   `AssistantMessage { turn }`, minted by the log (`Memo::next_id` over
   `id_floor`), never chosen by a caller.

2. **A cut is addressed by an id.** `ContextOp::Evict { through, note }`:
   every resident turn with id `≤ through` leaves, except a user turn whose
   exchange still has an assistant turn above the cut, which stays with its
   survivors (`cut_departures`, `has_survivor`). So `through` at a user turn
   removes everything before that exchange and nothing of it; `through` at an
   assistant turn cuts into its exchange, keeping its prompt. The invariant
   this preserves: the first resident turn is a user turn. An import's opening
   or an abandoned prompt is an ordinary turn and leaves when named or passed —
   no special case.

3. **The newest turn survives structurally.** `plan_eviction` draws its
   candidates from every resident turn *but the last*, and `validate_edit`
   refuses that one by name. `is_live_exchange` stops being an eviction
   concept; it survives only where it belongs — the door refusing to read the
   exchange still being written, naming that exchange's own closed turns,
   which *are* readable (`unclosed_turn` is the one turn no door reads back).

4. **Evictions stay prefix operations; the marker stays one message at 0.**
   `render_head` is a pure function of the table and its cuts, so between two
   cuts message 0 is byte-stable and the cache arithmetic of
   [[decisions/260906_context-rollover|context-rollover]] is unchanged.
   Reading order in the marker: index of what left → the task → recent work.

5. **Weigh at every turn boundary.** `Avatar::evict` runs at the top of every
   `deliberate` loop iteration, before `record_turn_start`, never at entry. An
   eviction can no longer take the work in hand, so there is no point in the
   loop where weighing is unsafe.

6. **The pressure reminder rides the steering channel.** At a tool boundary
   under pressure, the edge-triggered, budget-free reminder joins the one
   steering message the protocol admits after a batch (`append_steering`),
   naming the turn the next boundary would cut through.
   `Nudges::pressure_reminder` keeps the one latch; a `--chat` trunk, holding
   no `Nudges`, is never told.

7. **A child inherits the table.** `Protocol::Inherited { source, through,
   turns, cuts }` carries the parent's whole table at the fork (every turn
   `kind: Inherited`, `held` as the parent had it) together with the cuts made
   in it; the imported `ContextMessage { id, exchange, message }`s then
   re-record the resident ones as this log's own under the same ids
   (`inherit`, zeroing a resident turn's weight because its own records
   follow). The child's marker, survey and index are the same projections of
   the same fold; nothing marker-shaped crosses by value.

8. **The door speaks turns.** `` transcript `read [exchanges, turns] `` and
   `` transcript `grep [pattern, exchanges, turns] `` — a record with optional
   fields, on the `` `grep `` precedent. `` context `survey `` and
   `` transcript `index `` answer turns, not exchanges.

9. **No summariser, no gap marker, no synthetic exchange.** The note is the
   model's, the index is ours; two marker kinds for one fact is a defect; a
   fabricated prompt violates
   [[invariants/turn-ends-ready|exchange-ends-ready]].

## The data model

```rust
pub struct Turn { pub id: u64, pub exchange: u64, pub kind: TurnKind,
                  pub label: String, pub bytes: usize, pub held: Held }
pub enum Held { Resident, Evicted, Dropped }
pub struct Cut { pub through: u64, pub note: Option<String> }
pub struct Folded { state: State, turns: Vec<Turn>, cuts: Vec<Cut> }
```

- **`Turn` is the row; there is no row type.** `label` (`OPENING_CHARS`, 50) is
  set as a turn opens; `bytes` is summed by the fold as the turn's own records
  land, from the same `message_bytes` the render computes — so a resident
  weight, a departed weight and a marker fragment's weight are one sum.
  `record::ContextRow`, `ContextSurveyItem`, `StoreIndexItem`, `EvictedRow` and
  `span_row` all dissolve into it: `Display::Context { rows: Vec<Turn> }`
  carries the survey's own rows, and `turn_row` at the desk is what both
  `` `survey `` and `` `index `` answer with.
- **One table.** `turns` holds every turn this lineage recorded, in id order;
  the context is the `Resident` subsequence (`Folded::resident`); a cut or drop
  flips `held` and frees the same ledger slots. No `Span`, no model-fold
  `View`, no `departed` map, no stored marker rows, no `max_id` — the reach is
  `turns.last()`. (`record::View` survives untouched: that one is the
  *scrollback* fold, and never held model-context state.)
- **Placement lives in the ledger.** Each slot carries the id of the turn it
  belongs to (`Ledger::placement`, assigned by `record_turn`, recomputed by
  `refold`); runs are contiguous and ids monotone, so `Ledger::events(turn)` is
  two `partition_point`s. `Turn` carries no range. For inherited turns
  placement is foreign — which file, which loci — held in the lazily built
  `ancestry_index` and consulted by `read`/`grep` alone.
- **`Folded` is the fold's pure output** — what `refold` returns and what the
  `fold == memo` law of
  [[decisions/260812_context-is-a-projection|context-is-a-projection]] compares.
  `Memo` is the ledger, that `Folded`, `newest_edit`, and the caches under the
  recompute invariant: `render: HashMap<u64, TurnRender { end, segment }>`
  keyed by turn id, `head_render`, `ancestry_index`. `Rendered` is the
  assembled value the provider is sent — `Arc` segments and a byte total, one
  segment per turn.
- **Assembling an exchange.** A closed exchange whose own fold does not settle
  reads as the one `abandoned_note`; otherwise its turns' renders in order; the
  exchange in hand renders as it lies, since its last turn may still be growing
  and an abandoned exchange is not yet abandoned while it is the one in hand.
  `closed_messages` stays the one rendering of a whole closed exchange, and
  `into_chat_messages` over a turn's events is a turn's own material.
- **`apply_context_op(folded, op) -> Vec<u64>`** — the turns that left — is
  shared by `step_protocol` and `refold`, so a resume cannot disagree with the
  session it replays.
- **`render_head(turns, cuts)`**: for each cut in order, the `Evicted` turns in
  `(previous through, through]`, grouped into *fragments* — contiguous turns
  sharing an exchange, drawn with the exchange id, the user turn's label, the
  assistant-turn range and the summed bytes — then the cut's note,
  `Debug`-quoted. `HEAD_ROWS` (40) collapse and `OPENING_CHARS` are unchanged;
  one exchange may appear in two fragments across two cuts, and
  `departed_sentence` tells the whole departures from the partial ones.
- **`plan_eviction(keep) -> Option<u64>`**: candidates are every resident turn
  but the last. `spent` starts at the last turn's bytes plus, if it is an
  assistant turn, its own user turn's — a user turn is never a saving while its
  exchange has a survivor. Walk back: the first candidate that does not fit
  names the cut — itself, or, where it is a user turn, its exchange's newest
  resident turn (the amendment below). A plan that would take nothing is no
  plan and answers `None`.

The refusals name the state: "turn 350 has already left your context — the
earliest still in it is 351"; "turn 999 is not recorded — the latest is 420";
"420 is the newest turn; an eviction keeps the work in hand"; "exchange 12 is
still in progress — its closed turns 300–419 are readable with
`` transcript `read [turns: [300, 419]] ``".

## Amendments settled in implementation

Three points where the landed design departs from the plan's letter.

- **`Inherited` carries `cuts` as well as `turns`.** The fold is turns *and*
  cuts, and the child's marker must be the same projection of the same fold;
  without the cuts a child drew no marker at all and lost the parent's notes.
- **An overshooting user turn takes its whole exchange.** When the walk reaches
  a user turn whose exchange still has resident assistant turns and paying it
  overshoots `keep`, the cut is `through` that exchange's *newest resident
  turn*, so the exchange leaves whole. The letter — "the cut is `through` that
  turn's id" — would, under decision 2, take nothing of the exchange: the
  motivating scenario, one large old exchange, would never shed and the context
  would sit over the threshold forever.
- **`read` and `grep`'s two narrowings compose as a union.** A turn is reached
  when either `exchanges` names its exchange or `turns` covers its id;
  `` `grep `` with neither searches the whole transcript. An exchange reached
  whole renders through `closed_messages` and comes back byte-identical to what
  was sent; one reached partially answers its turns' own material
  (`Rendering::{Whole, Turns}`), and a fork-split exchange renders per turn
  because a mixture of two files' records would fold as abandoned. `` `grep ``
  skips only the unclosed turn, not the whole live exchange.

## What the human and the harness see

- The `/context` card groups the survey's resident turns into exchange runs at
  draw time (`context_exchanges`): label `exchange 12`, value
  `fix the parser…  turns 13–15 · 85 KB`. There is no live marker — the newest
  turn is the one an eviction structurally cannot name, so saying so would say
  nothing.
- Headless emits `num_turns`, `[turn n]` breadcrumbs and `[done] n turns`;
  synod emits `SynodEvent::Turn { id }`; the TUI's `Display::Turn` chrome and
  rail legend read "turn boundary".
- `append_user(text, continues)` and `Item::continues` survive: they
  distinguish steering that extends the turn in hand from a prompt that opens a
  user turn. Only the *eviction path's* `continues` and
  `plan_eviction_before` are gone — the exchange a nudge continues is the
  newest, and cannot leave.

## Accepted losses

- `` `read [turns: …] `` inside an *abandoned* exchange returns the turns' real
  material, where the context showed the abandoned note. Documented, not
  refused.
- An abandoned exchange's turns report their own weights in the survey while
  the context sends the one-line note. `total-bytes` stays the truth about what
  is sent, being `history_bytes` rather than a sum of the rows.
- Old `record.jsonl` files do not resume. No compatibility shim.

## Amended: the context is one structure

Landed the next day, in the commit that carries this paragraph. Every argument
above stands; what changes is what the one structure *says about a turn*. It
said whether the turn had left; it now says **where the turn is** — its records
in memory, or a pointer to the file and byte ranges that hold them. Eviction
moves a turn from here to there; an enquiry looks in the structure and, if the
turn is there, in the file. `Folded`/`Ledger`/`Cut`/`Held`-as-state become
`Context`/`Body`/`Pointer`/`Held { cut }`.

### The defect that forced it

`Folded` recorded that a turn had left, never which cut took it, so the marker
re-derived that from `(held, cuts[].through)` by an id window
(`cut_fragments`). The window is wrong wherever a cut leaves a survivor: a user
turn kept at cut *k* by the survivor rule of decision 2 and taken at cut *k*+1
has `id ≤ through_k`, so it was drawn under cut *k* — the wrong note — and cut
*k*'s row for that exchange gained that turn's bytes *after the fact*, an
already-sent row growing retroactively. Nothing ever vanished; the marker
misattributed, and a row's weight was not stable. The representation did not
record what a cut did, so the renderer had to guess. Adjacent to it, because
the marker was recomputed only at a cut (`recompute_head`), a drop that emptied
a partially evicted exchange left the sentence still saying "turns 4–5 of
exchange 3" when exchange 3 had left whole.

Behind the marker, three structures — `Ledger` by slot, `Folded` by turn id,
and a `Memo` of caches around them — were three partial views of one fact kept
in step by hand: `resume` needed a second fold (`Admission`) to judge the file
and a third (`refold`) to check the first two agreed, and "where are this
turn's bytes?" had two mechanisms (`Ledger.freed`, eager by slot; a lazily
built `AncestorIndex`, by turn, walking every ancestor file) plus a third
answer for a turn a seed re-recorded in part.

### The structure

```rust
pub struct Context {
    turns: Vec<Turn>,             // every turn this lineage recorded, in id order
    notes: Vec<Option<String>>,   // one slot per eviction; `Body::There::cut` indexes it
    state: State,
    source: PathBuf,              // own record.jsonl: where a turn here points once it leaves
    len: usize,                   // protocol records folded
    newest_edit: Option<usize>,
}

struct Turn { id: u64, exchange: u64, kind: TurnKind, label: String,
              bytes: usize, body: Body }

enum Body {
    /// In the context. `origin` is `Some` for a turn first recorded in an
    /// ancestor's file: `records` are what the seed re-recorded here.
    Here { records: Vec<Recorded<Protocol>>, origin: Option<Pointer> },
    /// Departed. `cut` indexes `Context::notes`; `None` for a drop.
    There { at: Pointer, cut: Option<usize> },
}

pub struct Pointer { pub source: PathBuf, pub loci: Vec<Locus> }
pub struct Row { pub id, pub exchange, pub kind, pub label, pub bytes, pub held: Held }
pub enum Held { Resident, Evicted { cut: usize }, Dropped }
pub struct Linked { pub row: Row, pub at: Pointer }
```

`Turn` and `Body` are private to `record/model.rs`. This revises "`Turn` is the
row; there is no row type" above: a turn holds records, so it is not a value to
hand out. `Turn::row` projects `Held` off the body — `Here` is `Resident`,
`There { cut: Some(c) }` is `Evicted { cut: c }`, `There { cut: None }` is
`Dropped` — and is the one way a turn leaves the structure as a row;
`Context::linked` is the one way it leaves as an address. `Row`, `Held`,
`Pointer`, `Linked` are the only public shapes, and `Display::Context` carries
`Vec<Row>`. `Held` carrying its cut is what makes the marker correct by
construction: the body says which cut took a turn, so nothing re-derives it,
and there is no second map to reconcile against the rows.

Everything else is a fold of that one structure, rendered on call and memoised
nowhere: `rendered()` (owned `Rendered { messages: Vec<ChatMessage>, bytes }`),
`history_bytes`, `render_head`, `context_survey`, `transcript_index`,
`plan_eviction`, `sourced`. `Ledger`, `Folded`, `Memo`, `Cut`, `TurnRender`,
`Ancestry`/`AncestorIndex`/`Break`/`Fault`, `Rendering`, `Admission` and
`refold` all go, and with them the model fold's *check* of the `fold == memo`
law of
[[decisions/260812_context-is-a-projection|context-is-a-projection]]: replay
still carries that law generically, but there is no second fold to compare a
resume against, because `Context::step` is the one fold and judges each record
before applying it — so a hand-edited or foreign file is refused by the very
function that folds a live one. What guards a read-back is `read_at`'s digest
check, at the moment of the read, where the risk is. `Fold for Model` keeps
`type Memo = Context`: the trait is untouched.

### The principle's two corollaries

- **Every protocol record acts on the structure** — it opens or extends a turn,
  moves turns out, or installs them. A record the fold would ignore is not
  protocol; hence change 5 below.
- **A turn's transcript copy is where it was first recorded.** A fork's link
  carries that address for every row, resident or not, so a turn a seed
  re-recorded only in part — the assistant turn whose tool call *is* the fork —
  still reads back whole, and evicting it later in the child points at the
  original copy, never at the short one. A grandchild's `at` already names the
  grandparent's file: the lineage is flattened at each fork, so nothing walks
  it, and `Inherited::source`/`::through` go.

### Accepted changes for the model and the user

1. **The door returns turn material, always.** `` transcript `read `` on an
   abandoned exchange answers its real records, not the note the context sent
   in their place — the door is *the record*, the note is what was sent. This
   promotes the first accepted loss above to a rule, and retires the
   whole-vs-turns axis with it (`Rendering`, `reached_whole`, `one_file`,
   `closed_messages`): a whole closed exchange no longer comes back through
   one rendering, which is the half of the third amendment above that goes. A
   turn a seed re-recorded in part reads back whole from its origin, where the
   child used to answer its own short copy.
2. **An unreadable ancestor file is a read error at the door**, named by path,
   not a memoised `Break` with a refusal grammar of its own. An unnarrowed
   `` `grep `` still passes over a turn it cannot reach — now at the read
   rather than at location time, a pointer being resolved without touching a
   file; a narrowing that names one is still refused.
3. **The link carries an address per row.** `Protocol::Inherited { turns:
   Vec<Linked>, notes }` grows by one `Pointer` per row — a path and one
   `Locus` per record the row holds; `Locus` derives serde.
4. **A root log recorded before this does not resume.** `Held::Evicted` carries
   its cut, so `Display::Context`'s rows change shape, and the bookends change
   class. No compatibility is owed and none is kept; a child log is never
   resumed.
5. **`SessionStarted`, `SessionResumed`, `SessionEnded` and `TurnStarted` are
   `Forensic`.** They were folded by nothing, and five functions carried an arm
   to say so; `record_turn` filed them under the last turn, which may already
   have left. They are durable evidence that is not model context, by the
   wiki's own definition ([[decisions/260814_one-seam-one-log|one-seam-one-log]]).

The cost paid for holding no memo: the context is built once per provider
request, on top of the clone per attempt at the wire door that
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] fixed
there; and the eviction trigger, weighed at every turn boundary, renders the
marker and walks each unsettled closed exchange's records for the one fact
`abandoned_note` varies on. Both accepted deliberately, against a memo field
that would have to be kept in step.

## What this supersedes

[[decisions/260906_context-rollover|context-rollover]] stands on
evict-don't-summarise, the head marker as one message at 0, evictions as prefix
operations, the cache arithmetic, and `transcript` as the door onto the record.
Superseded in part: the eviction's *atom* (exchange → turn), its *address*
(`through_exchange` → `through`), its *cadence* (entry → every turn boundary),
the model fold's `View`/`Eviction`/`EvictedRow`/`span_row` carriers, and the
words "window" and "store".

[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] stands
on its law — the provider-facing context is a persistent value of shared
segments, and the memo is a memo of a pure function at immutable arguments —
with its cache re-keyed per turn (`TurnRender`) rather than per span, and its
retroactive-tail split dissolved: `render_turn` takes no flags, and the
exchange in hand is assembled turn by turn like any other. The amendment above
then retires the memo itself: `Rendered` is owned and rebuilt per request, and
what survives of that decision's law is the wire door's one clone per attempt,
and the context being a pure function of the structure.

## See also

[[map/exarch/agent|agent]] (`evict` at the loop's top, the pressure reminder,
`digest.rs`'s gauges), [[map/exarch/builtins|builtins]] (`context` and
`transcript` as the model reads them),
[[internals/session-record|session-record]] (the fold and the turn table),
[[design/agents|agents]] (a `mnemon` child inheriting the table),
[[invariants/transcript-admission|transcript-admission]] (`Inherited`'s
admissibility, and where a cut may land),
[[invariants/turn-ends-ready|exchange-ends-ready]].
