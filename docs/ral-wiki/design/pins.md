# pins: a read/write noticeboard, the matrix's dual

**A pin is *state* a kit publishes to a keyed slot on a persistent register, the
in-place dual of an event appended to scrollback.** The transcript is an
append-only log of things that *happened*, sealed once written; a pin is *what is
currently true* — a task rollup, a build's status, the file under edit — and
state changes by being **overwritten**, not appended. exarch already redraws
*host*-authored state in place (the agent matrix, the `ctx%` gauge, the phase
bar — fixed marks, never streamed, never logged); a pin is the missing fourth
cell, the **same graphic grammar authored by the kit instead of the host**. The
register started write-only — a kit could publish state but never read it
back — and is now read/write: `pin-read` and `pin-list` answer from the same
mirror the nudge already kept, so the model (and a kit) can survey the board
it writes to. The full reasoning for the wire and its taxonomy is
[[decisions/260622_surface-pins-state|surface-pins-state]]; the read side is
[[decisions/260803_register-is-read-write|register-is-read-write]].

## The vocabulary

A pin rides the same `surface` channel as everything else, as a *disposition
wrapper* around an ordinary [[map/exarch/cards|`` `card ``]] — the wrapper carries
only **placement**, the card vocabulary and its decoder reused verbatim, and
this wire is unchanged by the read side:

- `` `pin [key, body] `` — write `body` to register slot `key`, **overwriting in
  place** on re-pin. The `key` is model-chosen and *is the datum's identity* —
  the thing the host cannot guess, save the one reserved key below.
- `` `unpin [key] `` — drop the slot (a finished plan clears its gauge). An
  **absent body** is the same as `` `unpin ``.
- `services` — the one **host-owned** slot. Ordinary model-authored
  `surface `` `pin ``/`` `unpin `` to it is rejected with a diagnostic
  (`reject_protected_pin`, [[map/exarch/shell-eval|shell-eval]]); the host writes
  it as durable [[design/residency|services]] are born and settle, so its
  legibility is the host's to keep, not the model's to overwrite. Reading it
  is unprotected: `pin-read "services"` answers like any other key.

On top of that wire sit four model-facing commands, one `pin-*` family:
`pin-set`/`pin-clear` are ral wrappers over `` `pin ``/`` `unpin ``;
`pin-read`/`pin-list` are the two Rust enquiries that make the register
legible back ([[map/exarch/builtins|builtins]]). "Set" says a pin overwrites a
slot, "clear" says it empties — not "add"/"remove", which would say the
register is a collection rather than one card per key.

So a kit holding evolving state pins one rollup and overwrites it, rather than
marching `tasks 0/3`, `tasks 1/3`, … down the scrollback — the streaming the rail
doctrine forbids. `exarch/data/agent.ral` (the tasks section) is the first client: `transition` reads the
list back, computes the next one, and pins the gauge that *fills in place* —
a per-task `open → done` move appends nothing to the transcript, and now
appends nothing to a bound list either.

## How it flows

One decoder, one new arm, one viewport field, and one enquiry class over the
same mirror — no new concurrency invariant, because a pin is still emitted
**in-run** through the live foreground sink, the exact place a card is
already safe:

- **Decode.** `value_to_pin` is tried first on the surface channel, ahead of the
  `io` and `card` arms ([[map/exarch/shell-eval|shell-eval]]); they cannot collide
  — `io` is a `Map`, the rest distinct `Variant` labels. It resolves to
  `Surface::Pin { key, card }` or `Surface::Unpin { key }`; the applier records
  a forensic breadcrumb and publishes the corresponding live `Transient`.
- **Register.** The slots live on the [[map/exarch/frontend|`Viewport`]] as an
  *ordered* `key → Card` map; a pin is `set_pin` (overwrite or insert, first-seen
  order), an unpin is `drop_pin` — the in-place analogue of `push_card`, touching
  neither the flatten nor the log. `reset` clears it on `/clear`, so pins are
  generation-bounded exactly as scrollback is.
- **Mirror.** The session keeps its own copy alongside the viewport's, per
  agent: a `key → PinDigest` map (`PinDigests`, [[map/exarch/shell-eval|shell-eval]])
  holding the full decoded `Card`, written on every accepted pin/unpin. It was
  born to let the nudge name what is pinned without parsing rendered text; the
  read side reuses the same store rather than adding a second one — `pin-read`
  and `pin-list` are enquiries answered straight from it.
- **Render.** The register is a **reserved right-hand column** for the *focused*
  session — a flat strip glued to the right edge, never a floating overlay that
  would occlude the yank-able scrollback. It claims only dead margin past the
  `READ_W` reading cap, so the transcript never narrows; below a width threshold
  it collapses to a one-row **pin band** beside the matrix. The frame reads
  symmetrically: the **rail owns the left edge** (what happened), the **register
  the right** (what is).
- **Headless.** There is no drawn register to overwrite, so a pin renders nothing.
  The record log retains `Pin`/`Unpin` as forensic breadcrumbs
  ([[decisions/260814_one-seam-one-log|one-seam-one-log]]) that no fold
  draws — the live register follows the shell boundary and is not restored
  on resume. Pinned state is ambient, like the
  matrix. `pin-read`/`pin-list` still answer headless, since they read the
  mirror, not the drawn column.

## Reading the register back

`pin-read <key>` answers the card stored under `key`, **canonically
re-encoded**, or `unit` on a miss; `pin-list` answers the occupied keys. The
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
`pin-read`.

Read-after-write within one run is sound for free: `DeskBinding::enquire`
drains queued surface frames before answering a request
(`exarch/src/fleet/desk.rs`), so a pin written earlier in the same script is
already in the mirror when the read is answered. Both enquiries are per-agent,
same as the mirror they read — a sub-agent's register is its own, and
`pin-read` never crosses that line; a foreign-key read is deliberately out of
scope (see the ADR's open questions).

## The model is the register's default mutator

Because the register now reads as easily as it writes, the model can survey
the board (`pin-list`), read a slot (`pin-read`), judge it, and revise it
(`pin-set`) with no kit in the loop — a stray fact, a warning, a note is
pinnable with zero apparatus, the same way any card is surfaceable with zero
apparatus. A kit that *owns* a key instead treats its card as a
serialization: read it back, destructure against its own schema, mutate,
`pin-set` again. The tasks kit is the worked example
([[decisions/260803_register-is-read-write|register-is-read-write]], §4): its
mutators take no list argument and thread none — `add-task`, `transition`,
`tag-task` each read `pin-read "tasks"`, decode it against the kit's own row
shape, and write the new rollup back through one `sync-tasks` write point,
which clears the slot once no task remains open. A card under `"tasks"` the
model wrote directly, in a shape the kit's decoder does not recognize, fails
the next kit call with a didactic `fail` naming the expected shape — the
price of one keyspace shared between a schemaless mutator and a schema'd one.

## The model watches its own pins

Because pinned state is something the *user is watching* on the rail, the
[[map/exarch/agent|nudge]] facility keeps the model restless about it. The
agent keeps the same small `key → one-line summary` mirror `pin-read` answers
from, while the session is otherwise pin-blind and the
events go straight to the frontend. There is **one** pinned-state nudge,
uniform for every pin kind (a task, a goal, any other pinned state
alike) and every agent role (the interactive trunk and a returning sub-agent
alike): while anything is pinned, a **budget-free** reminder fires on every
clean completion, naming the pinned state; with nothing pinned, a gentler,
throttled reminder suggests `set-goal`/`add-task` instead. The exception is
*actionability*: while the agent has live descendants, the pin/no-pin reminder
waits for their results, because the agent has already delegated the next
move. This nudge is independent of, and additive with, a *returning* agent's
separate obligation to call `reply` — neither suppresses the other, so a
sub-agent that finishes without replying while it still holds live pinned
state is reminded of both once it is not waiting on children. This is the
discipline pinning earns: a kit that publishes state to a slot the user watches
is reminded to keep it true, whether the agent holding it is the trunk or a
sub-agent.

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
side this page now describes: `pin-read`, `pin-list`, the encoder, the
canonical-form rule, and the tasks/goal kits as preludes over the family),
[[decisions/260619_surface-carries-documents|surface-carries-documents]]
(the `` `card `` body a pin reuses verbatim),
[[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]] (the matrix
this is the model-authored dual of, and the encode-don't-stream doctrine),
[[map/exarch/cards|cards]] (the render document the body decodes through, and
the encoder that inverts it), [[map/exarch/builtins|builtins]] (the
`pin-set`/`pin-clear`/`pin-read`/`pin-list` family and the tasks kit built over
it), [[map/exarch/frontend|frontend]] (the viewport register and the draw
layout), [[map/exarch/shell-eval|shell-eval]] (the host sink, the pin-first
decode, and the mirror `pin-read` answers from), [[map/exarch/agent|agent]]
(the nudge that reminds the model of its pins, from the same mirror
`pin-read` reads), [[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]
(the commitment keyspace retired, leaving `services` the one protected slot),
and `exarch/data/agent.ral` (the tasks section — the first client, now a pure
prelude over the family).
