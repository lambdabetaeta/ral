---
status: active
generated_at_commit: 4bc5006c
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
  placement is foreign — which file, which stamps — held in the lazily built
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
exchange in hand is assembled turn by turn like any other.

## See also

[[map/exarch/agent|agent]] (`evict` at the loop's top, the pressure reminder,
`digest.rs`'s gauges), [[map/exarch/builtins|builtins]] (`context` and
`transcript` as the model reads them),
[[internals/session-record|session-record]] (the fold and the turn table),
[[design/agents|agents]] (a `mnemon` child inheriting the table),
[[invariants/transcript-admission|transcript-admission]] (`Inherited`'s
admissibility, and where a cut may land),
[[invariants/turn-ends-ready|exchange-ends-ready]].
