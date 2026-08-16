---
status: accepted
---

# Context is a projection of the log

**The log is the session; model context is its projection.** `events.jsonl` is
an append-only record of protocol events and context edits. The in-memory
`Projection` is a memo of folding that record, not a second authority. This
joins the durable identity of [[decisions/260623_recording-follows-the-event|recording-follows-the-event]]
to the context vocabulary: an edit changes what the model sees by recording an
event, never by mutating a hidden mirror.

## The law

The exact law is: **no recorded event is ever removed**. A completed JSONL line
is a recorded event; an unterminated final line is a crash fragment, not an
event. Resume may quarantine that fragment in `events.jsonl.crash` and trim the
live file after the sidecar is durable, because it is repairing a write that
never became a record. It may not discard a complete line. `/clear` rotates the
records and starts a new ledger; it does not truncate the old one. There is no
in-place compaction of the durable history.

The law is about a durable record. A `--no-logs` session deliberately has no
`events.jsonl`, no transcript, no lock, and no rotation. Its mirror is
transient, so the law is out of scope rather than breached; children inherit
that election. A transient session has no durable identity and cannot be
resumed.

## One state, one fold

The log is the only state. Protocol state, exchange spans, the running maximum
exchange id, the newest context-edit index, and the current digest are all
derived by a left fold over the recorded sequence. `record` appends an event
and takes one step of that fold; replay takes the same steps from the first
line. This is **fold-as-spec, incremental-as-implementation**: the
from-scratch fold states what the session means, while the memo keeps live
operations O(1) per ordinary event and O(spans) per edit.

Every prior "keep the mirror consistent" obligation is now this derivation at
`record`, not a second state to synchronise.

That statement has two separate proof obligations:

- `fold(log) == memo` proves the wiring. It catches a record path that wrote an
  event but failed to advance the projection, or advanced it with the wrong
  event.
- An independently batch-built reference proves the step itself. Equality
  against a reference made from the same step is vacuous: a shared step
  reproduces its own defects. The reference therefore has its own state
  transition and message projection.

The old authoritative fields are gone. `State`, `View`, `Digest`, and the
exchange maximum live inside one derived memo; byte ranges are I/O metadata for
events which no longer need residency, not another semantic state. The fold
also repairs the one legitimate protocol seam: only the final live span may
dangle, and a resumed or inherited history refuses foreign or malformed
identity rather than synthesising an unrecorded event.

## Identity is recorded

An exchange id is resolved once by the writer and written into every event
which needs it. Replay reads that id; it does not infer identity again from
position, the last visible span, or a pretty transcript. Continuation therefore
uses the running maximum in the log, even after the latest exchange has been
dropped from the view. Steering carries the current live exchange id as well.
The old on-disk ambiguity between a steering event and a top-level event dies
with this rule.

Edits address the model view by whole, closed exchange. The model may edit only
at a protocol boundary; a live exchange, including a giant tool result still
being assembled, is untouchable until quiescence. `ContextEdited` records
`Fold` or `Drop` and its authority, then the fold changes the projection. There
is no model-facing rewind operation: model rewind is `context-drop`, a range of
closed exchanges. The user's `/rewind` command validates the ready-boundary
anchor and desugars to the same `Drop`, sheds queued self-nudges (a user post,
a command, or a worker result already queued survives), and resets the nudge
budget. The shell does not resume with the model: `--resume` replays the
event ledger while booting a fresh shell and imports a note describing the
shell state that was not durable.

## The price per regime

Residency follows the view (D8). The edit step frees events as soon as their
spans leave the model view and retains only byte ranges into `events.jsonl`; a
replay performs the same step. Thus resident memory is O(view), plus a small
amount of span/range metadata, in every regime, including transient
`--no-logs`; that mode discards removed spans instead of spilling them.
The provider serialises the whole surviving view, so spilling a live span
would add a production read path for material the next request already needs.

That bound is safe because two decisions meet:

1. the context vocabulary addresses the **view** — `context` reports spans and
   digests by exchange reach, and `context-read`/`context-drop` cannot name an
   event which is merely forensic or an exchange swallowed by a digest; and
2. the disk log is the **identity** — a spilled span is still an exact byte
   range in the append-only ledger, and the fold is the only operation allowed
   to read it.

The memory bound does not bound the file. Calibration found 956 events at
1.4 MB, about 1.5 KB per event; compact JSONL saves little because strings
dominate. The observed rates are rates, not admission bounds: interactive use
adds roughly 1–5 MB/day, while a saturated autonomous agent adds 40+ MB/day
(300–500 MB/day if every tool-result cap is hit). The price is disk and O(file)
resume time. Segment rotation is the named relief: deferred, and expected to
stay deferred until resuming a mammoth ledger becomes a practical problem.
`/clear` already supplies the safe rotation boundary.

The incremental fold removes the former quadratic live cost in every regime.
`state`, `view`, and `next_exchange` read the memo; `history_bytes` and provider
projection still serialise the bounded view. Resume pays the one full fold of
the file, and the implementation also compares that fold with the incremental
replay before admitting the session.

## Recording follows the event

The extension to [[decisions/260623_recording-follows-the-event|recording-follows-the-event]]
is precise: `events.jsonl` is promoted from *the model view* to **the log whose
fold is the model view**. Membership is decided by one question: does this
event determine the projection, or is it a forensic breadcrumb? Both belong in
the log; neither is a reason to maintain a second state. This is the honest
version of a criterion the old file had already failed. `Compacted` recorded a
fact that a compaction happened but never recorded its cut, so the old durable
file could not replay into the post-compaction view. `ContextEdited` applies the
correct rule to the new vocabulary: its operation and authority are recorded,
and the fold can reconstruct the edit.

`transcript.jsonl` remained the operational projection for a user-facing
trace, and was not a second model state — until
[[decisions/260814_a-trace-is-a-fold|a-trace-is-a-fold]] deleted it outright, paying the
unification debt in full rather than keeping the two records. The amended
recording ADR names the rent this page had assumed would persist.

## Superseding note: the log generalises, the law survives

[[decisions/260814_one-seam-one-log|one-seam-one-log]] promotes this page's
principle from the model view to the log as a whole. `events.jsonl` is
retired into `record.jsonl`, one log carrying three record classes, and
*every* durable artifact is now a fold of it — the model context over the
protocol class, the scrollback over the display and forensic classes, the
rendered `user.log` a regenerable render of the latter. The law migrates
intact: no recorded fact is ever removed, `/clear` rotates, resume
quarantines the torn tail, and `fold == memo` is carried once by a generic
replay driver rather than once per consumer — the fold-as-spec discipline
this page established, now stated for two folds instead of one.

One precision the superseding page insists on: the *view* fold exists and
obeys the same law, but it drives only resume — the live frame remains an
accumulator over the bus stream, for the reasons enumerated there. The
projection principle governs everything durable; it does not yet govern the
live screen.

## Accepted losses

The design accepts the following costs and boundaries rather than smuggling
them back as mirror-maintenance rules:

- Shedding the live exchange is the headline limit. Closed-exchange edits keep
  the provider protocol valid, but a large result cannot be removed until the
  exchange reaches quiescence.
- The vocabulary addresses the view, not the forensic log. Dropped and folded
  spans are intentionally not a queryable history store; `context-read` before
  a drop is the sanctioned handoff, not restoration or undo.
- The edit vocabulary is monotone: no un-edit, restore, or undo. A fold is a
  prefix operation with one digest, so mid-history folds and multiple digests
  are deferred; imports inside the reach are swallowed by that digest.
- User-side `context-drop` and `context-fold`, step-granularity edits, wire or
  synod resume, resumed child logs, descendant cancellation on `/rewind`,
  structured-frame handoff, and model-facing `context-rewind` are out of v1.

The file's unbounded growth is also accepted. Segment rotation would preserve
the law by sealing a segment and carrying its fold into a new one, but it adds
another resume and identity protocol. The trigger for revisiting it is a
resume whose O(file) fold is materially mammoth, not a theoretical dislike of
append-only storage.

See [[map/exarch/agent|agent]], [[map/exarch/builtins|builtins]],
[[map/exarch/frontend|frontend]], and [[design/agents|agents]] for the
operational surfaces.
