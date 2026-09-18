---
status: accepted
generated_at_commit: ced3518c
---

# An eviction is a set of turns

**There is one context edit. It takes a set of resident turns out of the
context, wherever they lie, leaves a marker standing where they were, and
carries the model's optional note. Every way of addressing the context and
the transcript speaks turn ids, as a list.** `` context `evict [turns: [Int],
note: Str] `` is the model's spelling; `/rewind <turn>` and the harness's own
pressure eviction are the same record under a different `EditAuthority`.

## Why

The previous vocabulary had three edits that were one operation seen through
three addresses: `` `evict [through: n] `` took a prefix, `` `drop
[exchanges] `` took whole closed exchanges and left no marker, `/rewind n`
took a suffix by exchange. [[decisions/260812_context-is-a-projection|context-is-a-projection]]
had already named the prefix/set asymmetry as unjustified;
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] made the
prefix edit turn-granular and left the set edit on exchanges.

What none of the three could do is the case that matters most to an agent:
a useless detour *inside the live exchange*. The model reads a file it did
not need at turns 41–43, knows it by turn 44, and wants those three turns
gone with a line saying why. `evict` would take the prompt and the good work
before them; `drop` could not touch the exchange being written and could not
name a turn; neither carried a note that stood *where the hole is*.

## The decision

1. **One edit, one record.** `Cut { turns: Vec<u64>, note: Option<String> }`
   is recorded as `Protocol::Evicted { cut, by }`, with the display twin
   `Display::Evicted`. `ContextOp`, its `Drop` arm, `EvictionPlan` and
   `Held::Dropped` are deleted; `Held` is `Resident | Evicted { cut }`.

2. **The recorded set is the resolved set.** The writer applies the survivor
   rule and records the ids that actually left; replay departs exactly those
   ids and judges each one resident, re-deriving nothing. This is
   *identity is recorded* from
   [[decisions/260812_context-is-a-projection|context-is-a-projection]]
   applied to the edit.

3. **Two guards, one silent rule.** Refused by name: a turn not recorded, a
   turn that has already left, and the *unclosed* turn — the one being
   written, which exists only while the protocol is mid-exchange. At
   `ReadyForUser` nothing is unclosed, so a user rewind may empty the context.
   Kept silently: a user turn whose exchange still has a resident turn not in
   the set. Hence the invariant every resident assistant turn has its prompt
   resident before it, and the context opens with a user message. A set that
   would take nothing is refused with the reason.

   "The live exchange is untouchable" was never the invariant; it was the
   coarsest address at which the real invariant — *no edit touches the
   unclosed turn* — could be stated. `is_live_exchange` leaves the fold.

4. **A marker per hole, in place.** A hole is a maximal run of departed
   turns in the table. Each renders as one bracketed user-voice message at
   its own position: the sentence of what left, one row per exchange
   fragment grouped by the cut that took it, that cut's note beneath its
   rows, and how to read the turns back. The former head marker is the hole
   at position 0; there is no second marker kind
   ([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] §9). A
   cut adjacent to a hole joins it; a late cut with resident turns between it
   and the head hole leaves message 0 untouched, and the provider re-reads
   from the hole, not from the start.

5. **The user's rewind leaves a marker with no note.** The harness states
   facts about the conversation; a model that knows a path was rewound does
   not walk it again. `/rewind <turn>` takes every resident turn from that id
   on.

6. **The read door speaks the same address.** `` transcript `read [turns:
   [Int]] `` and `` transcript `grep [pattern, turns: [Int]] `` name turns by
   id; `exchanges:` is gone from both, as is the `[from, to]` pseudo-range —
   ral's range primitive is `range a b`, which *is* a list of `Int`, and a
   turn address is a `[Int]` however it was built. The read answers one
   element per turn, `[turn, exchange, messages]`. What you can read, you can
   evict, by the same name.

7. **The pressure reminder names the set.** `Pressure::Over` carries the
   turns the planned cut would take; the reminder renders them as runs and
   offers `` context `evict [turns: !{range a b}, note: '…'] `` for the
   model to make the same cut with a note.

8. **A turn has a role; there is no exchange.** Every turn is `user` or
   `assistant`, and that is what every row says (`TurnRow { id, role, kind,
   label, bytes, held }`; `kind` is `own`, `import` or `inherited`). "The
   assistant turns answering prompt *u*" are the turns after *u* up to the
   next user turn — a function of role and order, derived where the survivor
   rule and the eviction plan need it, stored nowhere and shown to no one.
   The old `exchange` column was that memo leaking into the interface while
   the datum it derives from, the role, was smuggled as `id == exchange`.
   `UserPrompt { turn }` opens a user turn under its own id; `Steering
   { text }` extends the newest turn — a steering line after a tool batch, or
   a nudge continuing the prompt in hand — so the log says which happened
   rather than encoding it in a comparison with the reach;
   `ContextMessage { id, message }` names its turn alone. The marker's
   sentence is `Turns 1–29, 31–35 have left your context.` with one
   role-marked row per departed turn. The survey's receipt is `[rows,
   total-bytes]`: which turns have left is `` transcript `index ``'s `held`
   column, not a second count.

9. **Every tool result ends with `TURN: <id>`.** The id of the turn it
   closes — the same id the survey, the index, the marker and the TUI speak.
   Without it the model could address the past only after a survey; with it
   "the last three turns were a detour" is `!{range n-3 n}` directly. A
   fixed datum per turn, so the provider's cache is untouched.

## What it costs

A mid-history hole invalidates the provider's prompt cache from the hole
onward. For the detour case the hole is recent and the re-read small; the
docstring says so, and the survey's `bytes` lets the model weigh a dead read
against one cache miss. A marker adjacent to the next prompt yields two
consecutive user-voice messages, a shape the head marker already produced.

The shell and the filesystem are untouched, as they were by `/rewind`. A
rewound *read* had no effects; where a departed turn did, the note is where
the model says what still stands. Effect-undo would be shell machinery, and
this is not that.

## Supersedes

- [[decisions/260812_context-is-a-projection|context-is-a-projection]]:
  "model-facing rewind is out of v1", the monotone-vocabulary asymmetry, and
  "a live exchange is untouchable until quiescence".
- [[decisions/260906_context-rollover|context-rollover]]: the `` `drop ``
  tag and exchange addressing in the read door.
- [[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] §2 (a cut
  addressed by `through`), §4 (evictions stay prefix operations; the marker
  at message 0) and §8 (the door speaks exchanges and a turn range).
