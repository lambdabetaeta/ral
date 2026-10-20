---
status: accepted
generated_at_commit: ddde6ebc
generated_at_date: 2026-10-19
---

# An eviction is a span

**An eviction takes one contiguous run of the context — resident turns and
resident residues alike, in id order — and leaves one *residue* standing in
its place. A residue is a span `[from, to]` of turn ids, both ends
inclusive, and a note; it has no id of its own and is addressed by the ids it
covers. Residues nest by containment. The context is the *frontier* — every
turn no residue covers and every residue no later residue covers — in id
order, a residue rendered as one bracketed user-voice message carrying its
note and nothing else. The transcript family is the way back.**
`` exarch-context `evict [from: Int, to: Int, note: Str] `` requires a note
and may name any closed run, the live prompt included; a forced
eviction is one prefix span, has no note, says so, and may take the live prompt
too. Table order stays
id order, and `TURN: N`, ids unique and lineage-monotone, are unchanged. What
is given up: a set with a gap is two evictions, not one.

## Why

[[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]
let a cut take any set of resident turns and left a *marker* at each hole —
a maximal run of departed turns — re-derived at render time from the cuts
whose turns lay in it. Three costs followed from the marker being a
computation rather than a record:

- **It had no address.** The model could evict turns but not the marker
  standing for them, so old notes accumulated with no way to sweep them into
  one.
- **It was not bounded by the note.** A marker drew one row per departed
  turn, forty rows to a hole before collapsing — up to ~3 KB a hole, paid on
  every request.
- **It was re-derived.** Holes formed from cut indices at render time are
  where the inverted-range defect lived; the survivor rule, which kept a
  prompt while one of its answers stayed, made the sets scattered and the
  holes ragged.

A span fixes all three at once: it is recorded, it is addressable by numbers
the model already knows, and a residue's size is a constant plus its note.

## The decision

- **A residue is a record, not a row.** `Residue { span, note, by }`, exactly
  what its `Protocol::Evicted { residue }` carries, kept in creation order in
  the table (`residues`) and never removed. `Span { from, to }` is one type,
  never inverted: `Span::new` refuses `from > to`, deserialisation goes
  through it, and its `Display` is the one spelling, `turn 12` or `turns
  12–40`. The table of
  turns stays a `Vec<Turn>` in id order with nothing inserted. Its message,
  `render_residue`, is a pure function of the record — the sentence
  (`Turns 41–60 have left your context.`), the note `Debug`-quoted, and the
  read-back line — so it is byte-stable. A forced eviction's residue says it was left in a
  *forced eviction, without a note*. No rows: `` `index `` gives what lies
  under a residue on demand.
- **Covering, residency, holder, frontier.** A residue *covers* a turn whose
  id lies in its span, and an earlier residue whose span lies within its own
  (equal spans included: a rewrite covers what it rewrote). A departed item
  not withdrawn lies under its *holder*, the covering residue of smallest
  span (`Location::Under(span)`, `` `under [from: Int, to: Int] `` at the
  surface). Location is stored, a residue's as a turn's: an eviction stamps
  `Under` its span on every resident turn and every resident residue it
  covers, and its own residue stands resident; a withdrawal stamps
  `Withdrawn`. The stamp cannot go stale: the residue an item departs under
  is its holder for good, since any later residue covering the item covers
  that one whole, and withdrawal is permanent. The frontier is the resident
  items in id order. Spans are laminar — any two nested or disjoint — by the alignment
  rule below, which is what makes "smallest" well defined and the resident
  items disjoint.
- **One rule the model must learn: a run that reaches into a residue must
  cover it whole.** `from` must be an item start and `to` an item end: no
  residue has `r.from < from ≤ r.to` or `r.from ≤ to < r.to`. A larger run
  that covers old residues replaces their notes with the new one; the same
  span over a residue rewrites its note. Both ends must be recorded and not
  withdrawn, and the unclosed turn may not lie within. Refusals name the fix
  (`turns 41–60 stand together under one note: name from: 41, to: 60 whole,
  or leave it`).
- **A withdrawn run may lie inside a span.** A `/rewind` leaves withdrawn ids
  mid-sequence, and a forced eviction must span them: a withdrawn leaf
  inside a span stays withdrawn — withdrawal dominates — and is never
  rendered.
- **Inclusive ends.** `TURN: N` lets the model write `to: N` directly; a leaf
  is `from == to`; the record, the sentence and the fields agree with no ±1.
  The one half-open spelling is ral's `range a b`, a list constructor `read`
  takes: the residue's read-back line and the pressure reminder spell
  `!{range from (to+1)}`, and the teaching says so once.
- **Every row is a span.** `SpanRow { span, role, kind, label, bytes, location }`
  (`from` and `to` at the surface), `kind: ItemKind` being `own | import | inherited | residue`; a
  residue's role is `user`, its label its note. `` `survey `` answers the
  frontier's rows; `` `index `` every item ever recorded, by `from`, wider
  span first, later-recorded first, so a residue precedes what it covers;
  `` `grep `` searches turns' material and model notes (never the
  boilerplate, never a forced-eviction residue), a hit naming its item by `from`,
  `to` and `kind`, hits in `` `index ``'s order. `` `read [turns: [Int]] `` is unchanged.
- **A forced eviction takes one prefix span, and may take the live prompt.** Its
  plan (`plan_forced_eviction`) walks back over frontier items from the newest,
  which is never a candidate, spending `suffix_keep_budget`; the first item
  that does not fit ends the span, which runs from the frontier's first item
  through it. One ordinary `Evicted`, one residue, no note. Nothing in the
  plan or the fold knows what a prompt is: the live prompt is an item like
  any other, and `resolve_cut` has no case for it. The two authorities differ
  in one way — the model writes a note and the harness cannot — so the
  pressure reminder names the span planned as things stand, and a model that
  wants to keep what was asked evicts first under its own note.
- **The harness never rewrites.** A planned span that would be exactly one
  residue is no plan, and nothing is evicted: rewriting a note without a note
  saves nothing. A span over two or more items, residues included, merges
  them.
- **Reversed: the harness once kept the live prompt.** An earlier revision of
  this decision had the plan step over the live prompt (`current_prompt`) and
  cut two spans around it, `[first, p−1]` and `[p+1, to]`, so the prompt in
  hand stood verbatim between two forced-eviction residues. That is withdrawn: the
  cut never steps over the prompt and never makes two spans. The case
  [[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] was written
  for — a prompt whose work outgrows the window, the task in hand — is now
  relieved, with the task kept, only by the model's own eviction.
- **The survivor and opening rules go.** A user-voice residue standing where
  its run began makes "the frontier opens with a user-role item" true by
  construction, and a prompt may leave under a note while its answers stay.
  Contiguity replaces the survivor rule rather than adding to it.
- **`/rewind` anchors at an item start.** `Rewound { anchor }` withdraws by
  id as before; the anchor may not lie strictly inside a resident residue's
  span (`turn 50 lies under the residue over turns 30–80: rewind to turn 30
  or turn 81`). So a residue's leaves are all withdrawn or its `from` is
  not, and a rewind withdraws every residue whose `from` it withdraws.
- **The link carries residues.** `Protocol::Inherited { turns: Vec<Linked>,
  residues: Vec<Residue> }`; the child renders a residue from its own record.
  The judge checks the residues laminar and every row's `location`
  consistent with them — `location` is redundant with `residues`, and the
  judge keeps the redundancy honest. The child takes each turn's location
  from its row, and derives each residue's as the fold stamped it: withdrawn
  with its `from`, else under the smallest later residue covering it, if
  any.
- **Recorded identity.** The span is the resolved identity: the leaves it
  covers are the recorded ids in it at that point of the log, the residues
  it covers the recorded ones within it. `fold(log) == memo` holds; old logs
  do not resume.

## What it costs

- **Contiguity.** `[41, 43]` with 42 kept is two evictions. The survivor
  rule was what produced scattered sets, and it is gone.
- **The harness may take the task in hand.** In a forced eviction the live prompt
  can leave with the rest under a residue without a note; it stays readable
  through the transcript, but only the model's own note keeps what was asked
  in the context.
- **Rewinding to an evicted turn** works only at a residue's `from`.
- **Bounds improve.** A residue is ≤ ~400 B of fixed text plus 240 B of
  note, independent of what it covers; a forced eviction leaves exactly one
  resident residue.

Provider shape and cache arithmetic are as before: a residue is a user-voice
message between items, never inside a turn, never the last message while a
request is built; a cut rewrites the cache from its `from`, a note rewrite
from its residue, a forced eviction everything.

## Read against UniiChat

UniiChat (`Memory.ts`, `Server.ts`, `Tools.ts`, `prompt.txt` at `0500bf6`)
grows a complete binary tree over every message; a node is one line of at
most 512 bytes written by a separate compactor model, addressed by the span
`id+n` it covers and nothing else; the view is a frontier sent as text,
older nodes merging by age past a byte threshold; the model opens a node with
`zoom(id, n)`.

Taken:

- **Span addressing.** A summary needs no identity of its own; the span is
  the identity, in the ids the model already reads off `TURN: N`.
- **Nesting by containment.** A node over nodes is a bigger span; a residue
  over residues likewise, with no pointer.
- **One bounded line.** A residue is its sentence, note and read-back line.
- **Children on demand.** What lies under a residue is `` `index ``'s to
  show, never the residue's.

Not taken:

- **The harness does not compress the live prompt.** UniiChat compresses
  nothing of the live turn and lets a giant turn fail, which is the case
  [[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] exists for;
  exarch's forced eviction is one prefix span and may take the prompt with the
  rest.
- **`zoom`.** exarch's transcript family already reaches everything ever
  recorded: `index`, `read`, `grep`. No new tag.
- **The compactor.** Evict-don't-summarise
  ([[decisions/260906_context-rollover|context-rollover]]) stands: no
  provider call in the eviction path, so a forced-eviction residue carries no note.
- **History as text.** The recent half stays real messages, for the cache
  and for tool-call pairing.
- **The binary tree and age-based merging.** The tree exists so a mechanical
  compactor always has a next job; exarch's spans are chosen by the model or
  are the harness's cache-optimal half-prefix, and a laminar family of
  intervals is all the structure needed.

## Supersedes

- [[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]:
  "a cut is a set" becomes *a cut is a run*; §3 (the survivor rule and the
  emptied-set refusal), §4 (a marker per hole), §6's hit shape, and §8's
  marker sentence and `TurnRow` shape.
- [[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]: its
  marker and `Held { cut }` carriers. "One id space" stands, untouched.
- [[decisions/260906_context-rollover|context-rollover]]: the head marker's
  `HEAD_ROWS` bounds paragraph.

## See also

[[internals/session-record|session-record]] (the table, the frontier, the
render), [[map/exarch/agent|agent]] (the pressure plan and the reminder),
[[map/exarch/builtins|builtins]] (`exarch-context` and `exarch-transcript`
as the model reads them),
[[invariants/transcript-admission|transcript-admission]] (where a residue
may stand).
