# pins: a read/write noticeboard, the matrix's dual

**A pin is *state* a kit publishes to a keyed slot on a session-owned register,
the in-place dual of an event appended to scrollback.** A pin is *what is
currently true* — a task rollup, a build's status, the file under edit — and
state changes by being **overwritten**, not appended. It uses the same graphic
grammar as host-authored state such as the agent matrix. The register survives
exchanges, is emptied by `/clear`, and is not restored on resume.
`` exarch-pins `read `` and `` exarch-pins `list `` answer from the agent's
register (`Body.pins`) the nudge reads, so the model and a kit can survey the board
they write to. The taxonomy is explained in
[[decisions/260622_surface-pins-state|surface-pins-state]]; the read side in
[[decisions/260803_register-is-read-write|register-is-read-write]].

## The vocabulary

**`exarch-pins` is the register's only model-facing door**
([[map/exarch/builtins|builtins]]):

- `` `set [key: Str, body: Card] `` overwrites one slot with an ordinary
  [[map/exarch/cards|card]]. Every key is model-chosen; none is reserved.
- `` `clear <key> `` empties the slot, a no-op if it is already empty.
- `` `read <key> `` and `` `list `` make the same register legible back.

The family is foreground-only, like `exarch-agents` and `exarch-context`: a
call inside `spawn "…" { … }` errors rather than degrading.
`Surface::Pin` and `Surface::Unpin` are host recording vocabulary, not
source-level `exarch-surface` tags. Direct surface pin/unpin values are unknown:
they are dropped unrendered with a forensic diagnostic, not a model-facing
runtime error.

So a kit holding evolving state pins one rollup and overwrites it, rather than
marching `tasks 0/3`, `tasks 1/3`, … down the scrollback — the streaming the rail
doctrine forbids. `exarch/data/agent.ral` (the tasks section) is the first client: `` exarch-tasks `status` `` reads the
list back, computes the next one, and pins the gauge that *fills in place* —
a per-task `open → done` move appends nothing to the transcript, and now
appends nothing to a bound list either.

## How it flows

**The desk mutates the agent's register synchronously before publishing the
live UI update.** The flow is `exarch-pins` → typed `Request::Pins` → register
mutation → `surface::record` → key-only forensic breadcrumb plus transient →
live `Status` ([[map/exarch/shell-eval|shell-eval]]).

- **Enquiry.** The builtin decodes the family request. The desk's
  `apply_pin` borrows `Body.pins` through its foreground loan and inserts or
  removes the decoded `Card`. Read, list and nudge use this same register.
- **Record.** The desk constructs `Surface::Pin { key, card }` or
  `Surface::Unpin { key }`. `surface::record` emits key-only `Forensic::Pin` /
  `Unpin` and a live `Transient::Pin` carrying the card, or `Transient::Unpin`
  carrying the key. The surface decoder has no register arm.
- **Column.** The live [[map/exarch/frontend|`Status`]] column holds a copy
  of the `key → Card` slots in first-seen insertion order. `set_pin`
  overwrites or inserts; `drop_pin` removes. These slots are not scrollback
  blocks. `/clear` empties both this column and the agent's register.
- **Render.** The column is **reserved on the right** for the *focused*
  session — a flat strip glued to the right edge, never a floating overlay that
  would occlude the yank-able scrollback. It claims only dead margin past the
  `READ_W` reading cap, so the transcript never narrows; below a width threshold
  it collapses to a one-row **pin band** beside the matrix. The frame reads
  symmetrically: the **rail owns the left edge** (what happened), the **register
  the right** (what is).
- **Lifetime.** Card contents and live slots are not persisted. The log keeps
  only forensic pin/unpin keys
  ([[decisions/260814_one-seam-one-log|one-seam-one-log]]); control replay
  excludes forensic records, and view replay leaves these rows quiet. Resume
  starts with an empty register and `Status` column; its fixed note says pinned
  state is gone, not what pin history contained.
- **Headless.** A pin renders nothing, but `` exarch-pins `read `` / `` `list ``
  still answer from the agent's register, not the drawn column.

## Reading the register back

`` exarch-pins `read <key> `` answers `` `some `` of the card stored under
`key`, **canonically re-encoded**, or `` `none `` on a miss; `` `list `` answers
the occupied keys in sorted key order. The
encoder (`encode_card`, [[map/exarch/cards|cards]]) is `value_to_card`'s
inverse on the decoder's image, so what comes back is never the authored
bytes — a bare-string span or a bare mark, sugar the decoder accepts, comes
back tagged, and an unknown mark comes back as the plain text it already
degraded to. Canonical, not verbatim, closes the one hole a write-only
register never had to worry about: an authored value could otherwise carry
state the decoder discards, and the register would hold a truth the rail does
not show. Reading the canonical card makes storage and display one thing by
construction — the **WYSIWYG invariant**: only what the card renders can be
read back, and two states that render identically are the same state to
`` `read ``.

Read-after-write within one run follows from synchronous desk mutation:
`set` / `clear` updates `Body.pins` before returning, and a subsequent `read` /
`list` borrows that same register. It does not depend on draining surface frames,
on either carrier. Every enquiry is per-agent: a sub-agent's register is its
own, and `` `read `` never crosses that line.

## The model is the register's default mutator

Because the register now reads as easily as it writes, the model can survey
the board (`` exarch-pins `list ``), read a slot (`` `read ``), judge it, and
revise it (`` `set ``) with no kit in the loop — a stray fact, a warning, a
note is pinnable with zero apparatus, the same way any card is surfaceable
with zero apparatus. A kit that *owns* a key instead treats its card as a
serialization: read it back, destructure against its own schema, mutate,
`` `set `` again. The tasks kit is the worked example
([[decisions/260803_register-is-read-write|register-is-read-write]], §4): its
mutators take no list argument and thread none — `` exarch-tasks `add ``,
`` `status ``, `` `tag `` each read `` exarch-pins `read "tasks"` ``, decode
it against the kit's own row shape, and write the new rollup back through one
`tasks-sync` write point, which clears the slot only when the list is empty:
finishing every task keeps the board. A card under `"tasks"` the model wrote
directly, in a shape the kit's decoder does not recognize, fails
the next kit call with a didactic `fail` naming the expected shape — the
price of one keyspace shared between a schemaless mutator and a schema'd one.

## The model watches its own pins

Because pinned state is something the *user is watching* on the rail, the
[[map/exarch/agent|nudge]] facility tells the model when it changes. The
agent derives a one-line digest from the same `key → Card` register
`` exarch-pins `read `` answers from.
There is **one** pinned-state nudge, uniform for every pin kind (a task, a goal, any other pinned state alike) and
every agent role (the interactive trunk and a returning sub-agent alike):
**edge-triggered** and budget-free, it fires once, on the first quiet
completion after the register's one-line digest differs from the one last
told, and stays silent on every quiet completion that repeats an
already-told digest. An emptied register is itself silent — unpinning only
re-arms the edge, so a later re-pin fires again even at a digest told long
before, and there is no fallback reminder that nudges an empty register
toward `` exarch-goal `set `` / `` exarch-tasks `add ``: that
advertisement is what let a completion,
its own reminder, and the next completion cycle forever, and it is gone with
the livelock it caused. The exception is *actionability*: while the agent has
live descendants, the pin reminder waits for their results, because the
agent has already delegated the next move. Restlessness in a *returning*
agent is not this nudge's doing — that is its separate, budgeted obligation
to call `reply`, re-issued while unmet. The pin reminder itself speaks once
per change and then falls silent, so a sub-agent that finishes without
replying right as its pinned state changes is told of both in the same
completion, but only the reply obligation is told again next time. This is
the discipline pinning earns: a kit that publishes state to a slot the user
watches is told once when that state moves, whether the agent holding it is
the trunk or a sub-agent.

## Why this shape

- **It is the next honest cut on the road already taken.** documents → (operation
  vs appearance) → (render vs control) → (event vs state) → **(write-only vs
  read/write)**: each step refined the [[design/exarch-architecture|surface]]
  taxonomy by one distinction, and this is the first that looks *inside* the
  state cell rather than beside it.
- **It is the dual of a thing already on screen.** The register adds no new visual
  vocabulary — it lets a kit author what the host authors in the matrix
  ([[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]]), and
  now read it back the same way the host's own gauges are always legible.
- **Position carries the distinction.** Rendering state in a reserved column makes
  the plane's *horizontal* axis — Bertin's strongest variable — the event/state
  partition, legible in *where the mark sits* rather than implicit in which list
  it lands on.
- **It makes the doctrine expressible, and now enforceable.** "Encode the
  changing datum as a fixed-position magnitude, never streamed" was
  enforceable only on host marks; a write-only pin let a kit obey it in
  appearance while a bound list drifted out of step behind the scenes.
  Canonical read-back removes the second copy: the card *is* the datum, so
  there is nothing left to drift.

## See also

[[decisions/260622_surface-pins-state|surface-pins-state]] (the original
decision — wire format and taxonomy, write-only as first drafted),
[[decisions/260803_register-is-read-write|register-is-read-write]] (the read
side this page now describes: `` `read ``, `` `list ``, the encoder, the
canonical-form rule, and the tasks/goal kits as preludes over the family),
[[decisions/260619_surface-carries-documents|surface-carries-documents]]
(the `` `card `` body a pin reuses verbatim),
[[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]] (the matrix
this is the model-authored dual of, and the encode-don't-stream doctrine),
[[map/exarch/cards|cards]] (the render document the body decodes through, and
the encoder that inverts it), [[map/exarch/builtins|builtins]] (the
`exarch-pins` `` `set ``/`` `clear ``/`` `read ``/`` `list `` family and the
tasks kit built over it), [[map/exarch/frontend|frontend]] (the live `Status`
column and the draw layout), [[map/exarch/shell-eval|shell-eval]] (the host
recording vocabulary and the register `` exarch-pins `read `` answers from),
[[map/exarch/agent|agent]] (the nudge that reminds the model of its pins, from the same register
`` exarch-pins `read `` reads), [[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]
(the commitment keyspace retired, and its 2026-08-27 amendment records the
later removal of `services`, the one protected slot that survived it),
and `exarch/data/agent.ral` (the tasks section — the first client, now a pure
prelude over the family).
