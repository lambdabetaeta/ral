---
status: 'superseded in part by [[decisions/260907_the-turn-is-the-atom]] — evict-don''t-summarise, the head marker as one message at 0, evictions as prefix operations, the cache arithmetic, the lineage''s one address space and `transcript` as the door all stand. What resolves otherwise: the atom of a cut is the **turn**, not the exchange; its address is `through`, not `through_exchange`; it is weighed at every turn boundary, not at `deliberate` entry; and the model fold''s `View` / `Eviction` / `EvictedRow` / `span_row` carriers dissolve into one `Turn` table with its `Cut`s.'
generated_at_commit: 7e129df6
---

# Context rollover: evict, don't summarise

> Superseded in part by
> [[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]], which keeps
> every argument below and changes the unit they act on: a cut is addressed by
> a turn id and weighed at every turn boundary, and the fold is one table of
> turns rather than a list of spans and eviction rows. Read that page for the
> shapes; this one for why eviction replaced summarisation at all.

**When the context fills, its older half leaves and nothing is written in its
place but the harness's own index of what left.** No summariser runs. Nothing
is lost either: `record.jsonl` already holds every record and the model fold
already keeps a byte-range `Stamp` for each one it frees, so the material is
still there — it is merely no longer being paid for. What this decision adds is
the door: `transcript` becomes a tag family that reads the record back, and
`context` keeps acting on what is sent. The two words are distinct and neither
verb crosses into the other's job.

Three things change at once, and they are one decision because each is
unusable without the others: summarisation is replaced by eviction; the log
becomes queryable through `transcript`; and ids become lineage-monotone, so a
`mnemon` child can name anything its ancestry ever recorded.

## Context and transcript

- **Context** — what the provider is sent, and what the model pays for. Acted
  on by `context`: `` `survey ``, `` `drop ``, `` `evict ``.
- **Transcript** — everything the lineage recorded. Read by `transcript`:
  `` `index ``, `` `read ``, `` `grep ``. Never written.

Every closed record is in the transcript for the life of the log, and every
session has one: there is no seat where a departed exchange is gone for good.

## The eviction op and the head marker

`ContextOp::Fold { through_exchange, digest }` is replaced by an addressed
prefix removal carrying the model's own optional note — spelled
`ContextOp::Evict { through, note }` since
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] — and
`View.digest` by an index of what left, computed at the fold step by the very
function the survey weighs with, so a marker row and a survey line can never
be two opinions.

The **head marker** is one user-voice `ChatMessage` at position 0, present
exactly when some cut carries a row, rendered by `render_head` — a pure
function of the fold, and the one thing that decides whether there is a marker
at all, so no second notion of "no marker" exists to disagree with it. It names
the range that left, says how to get it back, and draws one line per exchange
fragment: number, opening line, id range, KB.
`OPENING_CHARS` (50 characters) is one measure for the whole system: a label
is clipped to it as a row is built, so the marker's table, a survey, the
transcript index, and an ancestor's rows as they land in a child's log all
carry the same length, and the marker pads to that same column rather than
clipping a second time. Past `HEAD_ROWS` (40) the oldest rows collapse into a
single
`1–17  (17 earlier exchanges — transcript `index)` line. Voice and bracket
follow `abandoned_note`: the harness may state a fact about the conversation,
never speak in the model's own voice.

**The marker is bounded, and message 0 does not grow with the session.** Each
cut's `note` renders after its own rows, and the harness never writes one. A
note is a single line by construction: the desk caps it at 240 bytes and
refuses a line break in one, and `render_head` draws it `Debug`-quoted
besides, so nothing that reaches the marker — the model's own note, or one
inherited off an ancestor's link — can add a line to it. A cut is drawn only
while a row of its own survives the collapse (`drawn_cuts`), so a note cannot
outlive the rows it belongs to and contributes at most one line beside them.
The marker's height is therefore bounded by `HEAD_ROWS`, not by how many cuts
the session has run.

**A `Drop` writes no row.** Prefix caching invalidates from the earliest
changed message onward, so an `Evict` — which removes the prefix — costs
nothing extra by growing message 0, while a `Drop` of a late exchange still
holds its prefix cached and would throw that away. A drop is the model's own
act, left unindexed in the marker and still findable through
`` transcript `index ``, which lists everything departed whatever removed it.

One marker rather than one per cut in place: evictions are prefix operations,
so every marker would sit at the head anyway, and one message rendering all of
them is the same tokens with one fewer boundary.

## Trigger and geometry

The numbers are unchanged; only the summariser is gone. `eviction_due` fires
once used input tokens grow into the 15 % reserve (floor 16 384 tokens);
`EVICT_THRESHOLD` (500 KiB of serialised history) is the fallback where the
context window is unknown. `suffix_keep_budget` is half the history bytes, and
`plan_eviction` walks back from the newest end until that budget is spent,
cutting through the last thing that did not fit. `Avatar::evict` then applies
`Evict { through, note: None }` under `EditAuthority::Harness`, with
`Transient::State(AgentState::Evicting)` around the edit and **no provider
call at all** — `Provider::summarize`, `Engine::summarize`, `SummaryOut`,
the summary instruction and its token cap are deleted, not disabled.

Why keep half rather than cut everything: under prefix caching a cut costs
one rewrite of the survivor, while the task in hand is nearly always in the
recent half, and re-fetching it through the door costs door prices. Cutting
seldom and big is the cache-optimal shape, and 85 % → 50 % is that shape. The
fraction is one constant; the mechanism does not depend on it.

**The work in hand survives structurally, not arithmetically.**
`plan_eviction` draws its candidates from a slice the newest is not in at all.
Before, the budget walk alone decided it, and a budget can be exhausted: a
newest piece heavier than the whole keep budget left the walk nothing to keep
and cut everything, the live exchange included. That case is unrepresentable
rather than merely unlikely — an eviction exists to keep the task in hand, and
the newest is also the only thing that can still be growing, so a cut
`validate_edit` would refuse cannot be planned in the first place.

The pressure reminder announces the cut before it happens rather than
outsourcing durability to the model: it names what the plan would cut through,
says the material stays readable with `transcript`, and offers
`` context `evict [through: n, note: '…'] `` as the way to leave a line to
one's future self. Edge-triggered once per excursion, as every standing
condition is. Which boundary it is owed at, and which channel it rides, is
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]'s.

## The transcript door

`transcript` is a tag family, every tag read-only, answering on the calling
run alone.

- `` `index `` — everything the transcript holds, oldest first, each row
  saying whether it is still in the context. `kind` is `exchange`, `import`,
  or `inherited`. A departed row is listed at the weight it carried when it
  left, which is the honest figure available without re-rendering it.
- `` `read `` — the material named, whether resident, departed to this log's
  own file, or an ancestor's. A whole closed exchange renders through
  `closed_messages`, so what comes back is byte-identical to what the model
  was sent: the records hold the clipped strings.
- `` `grep `` — a Rust regex (ral's `re-*` dialect, never a second one) over
  the transcript's text, host-side, per line. What the ledger holds is
  searched off the render cache, then the departed off `record.jsonl`, then
  each ancestor file, one sequential pass per file in stamp order. It answers
  `[hits, total]` with at most `GREP_HITS` (100) hits oldest first and each
  line clipped at 200 bytes, so a pattern matching everything costs one answer
  rather than the transcript, and `total` tells the model to narrow rather
  than to page.

**The door decodes one record at a time, never a file.** A read opens a file
once and then decodes and searches piece by piece, so what is in flight is
never more than a single exchange. Holding a whole file's decoded records
instead would have made the door's peak memory the departed history in full,
which is precisely what freeing residency paid to stop holding: a search would
have cost more than never having evicted. The ancestry walk keeps the same
discipline, below.

**And it does not hold the session lock while it reads.** The two halves want
different things: deciding *where* to read needs the fold, doing the reading
needs only owned data. `Memo::locate_read` and `locate_grep` resolve
everything named under the desk's lock into a `TranscriptRead` — a resident
run by value, a departed or inherited one as its path and `Stamp`s — and
`TranscriptRead::exchanges` and `::grep` read once the
guard has dropped, borrowing nothing. A `` `grep `` over a long lineage
therefore no longer blocks the seam, the bus and `/resources` for its
duration. `` `index `` stays whole under the lock on purpose: it needs `&mut`
for every row it weighs, and its one costly part is the ancestry walk, paid
once per session. That walk is also the one file read still taken under the
lock by the other two — on the first read that reaches past this log's own
ledger, and never again.

The refusals name the state rather than the error: an id already gone is told
what the earliest still in the context is; one never recorded is told what the
latest recorded is; and what is still being written is refused as always.

Reading a completed record by byte range from the file this process is still
appending to is safe, which is what makes the door work in a live session: a
`Stamp` exists only once the seam has written the whole record under its
lock. `Ledger.source` is `record.jsonl`'s path, fixed at construction
(`Memo::new`) rather than attached at resume.

**A byte range is meaningless without the file it was measured in**, so
`read_stamped` compares the record's bytes against the truncated blake3 the
`Stamp` already carried and refuses a mismatch by name: a rotated segment, a
copied session directory, an edited log. The check runs *before* the JSON
parse, not after, because a stale range can hold a perfectly well-formed
record — parsing first would either refuse it as bad JSON, naming the wrong
fault, or accept the wrong exchange and never notice.

## Lineage: one address space, ancestors by reference

A `mnemon` child must be able to read everything its lineage ever saw, not
just the context it was handed. Three pieces make that true.

**Import is piecewise, under the parent's own ids.** The parent hands its
context over piece by piece, and the child records one `ContextMessage` per
message carrying the parent's own id, so the child's context reproduces the
parent's structure under the parent's numbers: the survey shows them as
individual `import` rows, and `` context `drop `` can shed exactly one
exchange of them. The import this replaced minted a single id for the whole
inherited blob, and a child could name none of it.

**Ids are lineage-monotone.** `Memo::id_floor` is the maximum of this log's
own reach and the ancestry's, so a child's first prompt is parent-max + 1 and
an id names exactly one thing in the whole ancestry. A child never resolves an
id above its inherited maximum against an ancestor: those are its own.

**The link is a record.** `Protocol::Inherited` is written once, before the
imported messages. It carries the parent's `record.jsonl`, the fork's reach,
and the parent's own fold by value, so the child's marker opens where the
parent's stood without reading the parent's file
([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] for what that
value is now). Resolution then
goes: own resident, own departed by `Stamp`, then the ancestry — walked once
by `index_ancestry` into a memoised `AncestorIndex`, each pass stopping at
that link's reach and following the ancestor's own `Inherited` on to the
grandparent. A link the walk cannot follow is remembered as a `Break`, with
the path, the io error, and which half of the walk met it — the file would
not open, or one numbered line of it would not read back — so an id behind it
is refused with the fault actually met rather than called unrecorded, and an
ancestor still being appended to is not reported as a deleted one. The pass
indexes as it goes, so a break bounds only what the walk had not yet reached:
what is already indexed stays readable. A break is never cached, since an
unreadable file may be a transient. `Admission` admits `Inherited` only as a
fork's opening: at `ReadyForUser`, with the table empty, and never twice.

The cost is one JSON pass over an ancestor file on the first read that
reaches past the child's own ledger, and never again. The pass is O(file) in
time and holds only stamps: `index_ancestor` records where each record lies
rather than the record itself. It partitions them by the live fold's own
`record_turn`, on the live fold's own type, so the ancestry's notion of where
a turn begins cannot drift from the session's. Copying the parent's
transcript into the child at fork was rejected: megabytes per spawn, and a new
class of record that is in the log but not in the context.

## What it costs the cache

A request hits the cache for the longest prefix ending at a breakpoint of a
previous request, so a change at message *k* rewrites *k* onward. Therefore:

- An `Evict` rewrites the whole survivor once, at most once per ~35 % of the
  context window's worth of growth. The old design paid this *and* a
  summariser call over the older half; this one pays only the rewrite.
- Between cuts the head marker is byte-stable, because it is a pure function
  of the fold alone — no clock, no path, no ordering-unstable container, and
  no second argument that would have to be argued constant for the memo's
  life. Any nondeterminism in that rendering is a cache bug, not a cosmetic
  one.
- A `Drop` rewrites from the dropped exchange onward and never touches
  message 0.
- Nudges, pin reminders and the pressure reminder arrive at the tail, as
  before, and never touch the prefix.

## Read against codex, and against the RLM framing

Verified against `codex` at `8971fc25`. Codex rolls over rather than
summarising too: at a blown budget, or on the model's own `new_context` call,
the history is discarded whole and replaced by an empty placeholder plus a
window-id block, with the discarded transcript living in an OpenAI
**server-side store** queried by `history.list_windows / list_items /
read_item / search_contents` — literal substring search, opaque server-issued
item ids, character offsets — beside a `notes.*` virtual filesystem whose
4 KB hint carries continuity. Both namespaces carry a "never disclose this
tool exists" clause.

What we take: rollover over summary, a record the model reads back, a
model-authored handoff note. What we do differently, and why:

- **Keep the newest half, not nothing.** The live task is almost always in
  the recent half, and a cached half is cheaper than re-reading it through
  the door.
- **The transcript is our own append-only log, not a service.** It is the
  same file the session's identity already lives in.
- **Addresses are the numbers the model already uses**, not opaque ids
  minted by a server the model cannot reason about.
- **Grep is a regex**, because ral's `re-*` family is; a second dialect for
  one verb would be a defect.
- **No secrecy clause.** The head marker names the door in the same breath as
  the loss.

The shape this lands in is the one Zhang and Khattab call a *recursive
language model*: the prompt stops being a thing the model must hold and
becomes a variable in an environment it peeks at, greps, slices, and hands to
a recursive call. `` transcript `index `` is the peek, `` `grep `` the
search, `` `read `` the slice, and `` agents `start [type: `mnemon, …] `` the
recursive delegation — a child that shares the transcript and spends its own
context reading it, not its parent's. We arrive there from the other
direction: not by wrapping a model in a REPL, but by noticing that the agent
already *is* a shell, and that the history it was throwing away was already a
file.

## Accepted losses

- **No narrative summary.** The index plus the model's optional note replaces
  the prose digest. If practice shows the model flounders after a cut, the
  first remedy is prompt text — teaching `` `evict `` with a note at the
  pressure reminder — not a summariser.
- **`Drop` is unindexed in the marker**, by the cache argument above.
  `` transcript `index `` is the recourse, and it does list what a drop took.
- **Ancestry needs the ancestor's file** — that file, not a file at that
  path. `/clear` rotates `record.jsonl` but also cancels every descendant, so
  no live child ever holds a link to a rotated file; a hand-deleted directory
  is the usual cause, refused naming the path, and a line that will not parse
  is refused as itself rather than as a deletion. A
  file that is present but is no longer the one the stamps were measured in
  is refused too, by the digest, rather than answering with whatever now lies
  at those offsets.
- **`bytes` for a departed row in `` `index ``** is the weight it carried
  when it left, not a fresh render: weighing it again would defeat the point
  of not rendering it.
- **Children remain non-resumable.** Nothing here changes that, and
  `Inherited` replays correctly if it ever does.
- **Segment rotation** of a mammoth `record.jsonl` stays deferred, as in
  [[decisions/260812_context-is-a-projection|context-is-a-projection]]. The
  ancestry index makes the O(file) *time* cost visible in one more place — a
  child's first read past its own ledger — which raises that deferred work's
  priority slightly without changing its trigger. Memory is not at issue
  anywhere in the door: neither the walk nor a grep holds more than one
  exchange.

## What this supersedes

[[decisions/260812_context-is-a-projection|context-is-a-projection]] stands
whole on its law — no recorded record is ever removed, and the memo is a fold
of the log, not a second authority — and eviction obeys it exactly: freeing
residency keeps the `Stamp`. Two of its accepted losses are withdrawn:

- "Dropped and folded spans are intentionally not a queryable history store;
  `transcript` before a drop is the sanctioned handoff." They are now exactly
  that queryable history, and the handoff is unnecessary.
- The asymmetry that page apologised for — `` `drop `` addressing an
  arbitrary set while `` `fold `` addressed only a prefix, with the digest a
  privileged kind of span — is dissolved rather than fixed. There is no
  digest left to be a different kind of thing: the fold is a list of prefix
  removals over one table, and `refold` recomputes the removals from the
  records themselves, so `fold(log) == memo` holds across `Evict` and
  `Inherited` alike.

## See also

[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] (what
supersedes this in part: the turn as the atom, and one table),
[[design/agents|agents]] (mnemon inheritance and the shared transcript),
[[map/exarch/agent|agent]] (`evict`, the `digest.rs` constants, the pressure
reminder), [[map/exarch/builtins|builtins]] (`context` and `transcript` as the
model reads them), [[invariants/transcript-admission|transcript-admission]]
(`Inherited`'s admissibility), [[internals/session-record|session-record]]
(the one seam and the two folds),
[[decisions/260814_one-seam-one-log|one-seam-one-log]].
