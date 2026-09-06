---
status: active
generated_at_commit: c5df4203
---

# Context rollover: evict, don't summarise

**When the window fills, the older half of the closed exchanges leaves the
context and nothing is written in its place but the harness's own index of
what left.** No summariser runs. Nothing is lost either: `record.jsonl`
already holds every record and the model fold already keeps a byte-range
`Stamp` for each one it frees, so the material is still there — it is merely
no longer being paid for. What this decision adds is the door: `transcript`
becomes a tag family that reads the store back, and `context` keeps acting on
the window alone. The two words are now distinct and neither verb crosses
into the other's job.

Three things change at once, and they are one decision because each is
unusable without the others: summarisation is replaced by eviction; the log
becomes queryable through `transcript`; and exchange ids become
lineage-monotone, so a `mnemon` child can name any exchange its ancestry ever
recorded.

## Window and store

- **Window** — what the provider is sent, and what the model pays for. Acted
  on by `context`: `` `survey ``, `` `drop ``, `` `evict ``.
- **Store** — every closed exchange the lineage recorded. Read by
  `transcript`: `` `index ``, `` `read ``, `` `grep ``. Never written.

An exchange is *closed* once it is not the live one, *in view* while a span
for it is in `View.spans`, and *departed* once it has left. Every closed
exchange is in the store for the life of the log. `--no-logs` is the one seat
with no store, and says so where it matters rather than pretending otherwise.

## The eviction op and the head marker

`ContextOp::Fold { through_exchange, digest }` is replaced by

```rust
ContextOp::Evict { through_exchange: u64, note: Option<String> }
```

Every span with `id <= through_exchange` leaves the view. `View.digest`
is replaced by `View { evictions: Vec<Eviction>, spans }`, where an
`Eviction` is `{ rows: Vec<EvictedRow>, note: Option<String> }` and an
`EvictedRow` is `{ exchange, kind, opening, steps, bytes }` — one row per
span the op removed, computed at the fold step by the very function the
survey weighs a span with (`Memo::span_row`), so a row and a survey line can
never be two opinions.

The **head marker** is one user-voice `ChatMessage` at position 0 of the
view, present exactly when `evictions` is non-empty, rendered by
`render_head` — a pure function of the eviction list and one bit saying
whether this session keeps a log. It names the range that left, says how to
get it back, and draws one line per exchange: number, opening line clipped at
72 bytes, step count, KB. Past `HEAD_ROWS` (40) the oldest rows collapse into
a single `1–17  (17 earlier exchanges — transcript `index)` line. Each
eviction's `note` renders after its own rows; the harness never writes one.
Voice and bracket follow `abandoned_note`: the harness may state a fact about
the conversation, never speak in the model's own voice.

**A `Drop` writes no row.** Prefix caching invalidates from the earliest
changed message onward, so an `Evict` — which removes the prefix — costs
nothing extra by growing message 0, while a `Drop` of a late exchange still
holds its prefix cached and would throw that away. A drop is the model's own
act, left unindexed in the marker and still findable through
`` transcript `index ``, which lists every departed span whatever removed it.

One marker rather than one per eviction in place: evictions are prefix
operations, so every marker would sit at the head anyway, and one message
rendering all of them is the same tokens with one fewer boundary.

## Trigger and geometry

The numbers are unchanged; only the summariser is gone. `eviction_due` fires
once used input tokens grow into the 15 % reserve (floor 16 384 tokens);
`EVICT_THRESHOLD` (500 KiB of serialised history) is the fallback where the
window is unknown. `suffix_keep_budget` is half the history bytes, and
`plan_eviction` walks back from the newest closed span until that budget is
spent, cutting through the last span that did not fit. `Avatar::evict` then
applies `Evict { through, note: None }` under `EditAuthority::Harness`, with
`Transient::State(AgentState::Evicting)` around the edit and **no provider
call at all** — `Provider::summarize`, `Engine::summarize`, `SummaryOut`,
the summary instruction and its token cap are deleted, not disabled.

Why keep half rather than cut everything: under prefix caching a cut costs
one rewrite of the survivor, while the task in hand is nearly always in the
recent half, and re-fetching it through the door costs door prices. Cutting
seldom and big is the cache-optimal shape, and 85 % → 50 % is that shape. The
fraction is one constant; the mechanism does not depend on it.

The pressure nudge announces the cut before it happens rather than
outsourcing durability to the model: it names the exchange the plan would cut
through, says the material stays readable with `transcript`, and offers
`` context `evict [through: n, note: '…'] `` as the way to leave a line to
one's future self. Edge-triggered once per excursion, as every standing
condition is.

## The store door

`transcript` is a tag family, every tag read-only, answering on the calling
run alone.

- `` `index `` — every closed exchange the store holds, oldest first:
  `[[exchange, kind, prompt, steps, bytes, in-view]]`. `kind` is `exchange`,
  `import`, or `inherited`. A departed exchange is listed at the weight it
  carried when it left, which is the honest figure available without
  re-rendering it.
- `` `read <exchanges> `` — the named closed exchanges as material, whether
  in view, departed to this log's own file, or an ancestor's. Every route
  renders through the same `closed_messages`, so what comes back is
  byte-identical to what the model was sent: the records hold the clipped
  strings.
- `` `grep [pattern, exchanges] `` — a Rust regex (ral's `re-*` dialect,
  never a second one) over the store's text, host-side, per line. Resident
  spans are searched off the render cache, then departed ones off
  `record.jsonl`, then each ancestor file, one sequential pass per file in
  stamp order. It answers `[hits, total]` with at most `GREP_HITS` (100) hits
  oldest first and each line clipped at 200 bytes, so a pattern matching
  everything costs one window rather than the store, and `total` tells the
  model to narrow rather than to page.

The refusals name the state rather than the error: an id already gone is told
"exchange 7 has already left your context — the earliest still in view is
12"; one never recorded is told what the last closed exchange is; the live
exchange is refused as always; and under `--no-logs` a departed exchange is
refused with the mode named, the head marker having already said "They are
not readable: this session keeps no log."

Reading a completed record by byte range from the file this process is still
appending to is safe, which is what makes the door work in a live session: a
`Stamp` exists only once the seam has written the whole record under its
lock. `Ledger.source` is therefore fixed at construction (`Memo::new`) rather
than attached at resume.

## Lineage: one address space, ancestors by reference

A `mnemon` child must be able to read everything its lineage ever saw, not
just the window it was handed. Three pieces make that true.

**Import is per exchange.** The parent hands over its view as spans, and the
child records one `ContextMessage { exchange: <the parent's id>, message }`
per message. The partition rule opens a span per id strictly past the running
maximum, so the child's view reproduces the parent's spans under the parent's
numbers: the survey shows them as individual `import` rows, and
`` context `drop [7] `` can shed exactly one of them. The old import minted a
single id for the whole inherited blob, and a child could name none of it.

**Ids are lineage-monotone.** `Memo::exchange_floor` is the maximum of this
log's own running maximum and the ancestry's reach, so a child's first prompt
is parent-max + 1 and an id names exactly one exchange in the whole ancestry.
A child never resolves an id above its inherited maximum against an ancestor:
those are its own.

**The link is a record.** `Protocol::Inherited { source, evictions,
through_exchange }` is written once, before the imported messages. `source`
is the parent's `record.jsonl` (`None` when the parent ran `--no-logs`);
`evictions` is the parent's head-marker state by value, so the child's marker
opens where the parent's stood without reading the parent's file;
`through_exchange` is the fork's reach. Resolution then goes: own resident,
own departed by `Stamp`, then the ancestry — walked once by `index_ancestry`
into a memoised `AncestorIndex`, each pass stopping at that link's reach and
following the ancestor's own `Inherited` on to the grandparent. A link the
walk cannot follow is remembered as a `Break` — `NoLog`, or `Unreadable` with
the path and the io error — so an id behind it is refused with the reason
rather than called unrecorded. `Admission` admits `Inherited` only as a
fork's opening: at `ReadyForUser`, with `max_exchange == 0`, and never twice.

The cost is one JSON pass over an ancestor file on the first read that
reaches past the child's own ledger, and never again. Copying the parent's
store into the child at fork was rejected: megabytes per spawn, and a new
class of record that is in the log but not in the view.

## What it costs the cache

A request hits the cache for the longest prefix ending at a breakpoint of a
previous request, so a change at message *k* rewrites *k* onward. Therefore:

- An `Evict` rewrites the whole survivor once, at most once per ~35 % of the
  window of growth. The old design paid this *and* a summariser call over the
  older half; this one pays only the rewrite.
- Between cuts the head marker is byte-stable, because it is a pure function
  of `evictions` — no clock, no path, no ordering-unstable container. Any
  nondeterminism in that rendering is a cache bug, not a cosmetic one.
- A `Drop` rewrites from the dropped exchange onward and never touches
  message 0.
- Nudges and pin reminders arrive at the tail, as before, and never touch the
  prefix.

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

What we take: rollover over summary, a store the model reads back, a
model-authored handoff note. What we do differently, and why:

- **Keep the newest half, not nothing.** The live task is almost always in
  the recent half, and a cached half is cheaper than re-reading it through
  the door.
- **The store is our own append-only log, not a service.** It is the same
  file the session's identity already lives in.
- **Addresses are exchange numbers the model already uses**, not opaque ids
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
recursive delegation — a child that shares the store and spends its own
context reading it, not its parent's. We arrive there from the other
direction: not by wrapping a model in a REPL, but by noticing that the agent
already *is* a shell, and that the history it was throwing away was already a
file.

## Accepted losses

- **No narrative summary.** The index plus the model's optional note replaces
  the prose digest. If practice shows the model flounders after a cut, the
  first remedy is prompt text — teaching `` `evict `` with a note at the
  pressure nudge — not a summariser.
- **`Drop` is unindexed in the marker**, by the cache argument above.
  `` transcript `index `` is the recourse, and it does list dropped spans.
- **Ancestry needs the ancestor's file.** `/clear` rotates `record.jsonl` but
  also cancels every descendant, so no live child ever holds a link to a
  rotated file; the reachable cause of a missing `source` is a hand-deleted
  directory, refused naming the path.
- **`bytes` for a departed exchange in `` `index ``** is the weight it
  carried when it left, not a fresh render: weighing it again would defeat
  the point of not rendering it.
- **Children remain non-resumable.** Nothing here changes that, and
  `Inherited` replays correctly if it ever does.
- **Segment rotation** of a mammoth `record.jsonl` stays deferred, as in
  [[decisions/260812_context-is-a-projection|context-is-a-projection]]. The
  ancestry index makes the O(file) cost visible in one more place — a child's
  first out-of-view read — which raises that deferred work's priority
  slightly without changing its trigger.

## What this supersedes

[[decisions/260812_context-is-a-projection|context-is-a-projection]] stands
whole on its law — no recorded record is ever removed, and the memo is a fold
of the log, not a second authority — and eviction obeys it exactly: freeing
residency keeps the `Stamp`. Two of its accepted losses are withdrawn:

- "Dropped and folded spans are intentionally not a queryable history store;
  `transcript` before a drop is the sanctioned handoff." They are now exactly
  that store, and the handoff is unnecessary.
- The asymmetry that page apologised for — `` `drop `` addressing an
  arbitrary set while `` `fold `` addressed only a prefix, with the digest a
  privileged kind of span — is dissolved rather than fixed. There is no
  digest left to be a different kind of thing: the view is a list of prefix
  removals and a list of spans, and `refold` recomputes the removals' rows
  from the removed records themselves, so `fold(log) == memo` holds across
  `Evict` and `Inherited` alike.

## See also

[[design/agents|agents]] (mnemon inheritance and the shared store),
[[map/exarch/agent|agent]] (`evict`, the `digest.rs` constants, the pressure
nudge), [[map/exarch/builtins|builtins]] (`context` and `transcript` as the
model reads them), [[invariants/transcript-admission|transcript-admission]]
(`Inherited`'s admissibility), [[internals/session-record|session-record]]
(the one seam and the two folds),
[[decisions/260814_one-seam-one-log|one-seam-one-log]].
