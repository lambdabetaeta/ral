---
status: proposed
---

# The register is read/write: a pin stays a card, and `pin-read` reads it back

**[[decisions/260622_surface-pins-state|surface-pins-state]] gave a kit
somewhere to put what is currently true, and made it write-only. A drafted
revision — `` `pin `` carries a datum, a view in a typed *register handle*
renders it — bought static state/view agreement at the price of the register's
generality: every pin needed a handle, so pinning a stray fact, a warning, a
note acquired ceremony, and the noticeboard stopped being generic. This ADR
keeps the wire exactly as it is — `` `pin [key, body] `` where `body` is a
card, so anything the rail can show is pinnable with no apparatus — and adds
the read side: `pin-read` returns the card stored under a key in canonical
value form, `pin-list` names the occupied keys, and `pin-set`/`pin-clear` are
thin ral wrappers over the unchanged wire, completing one model-facing
`pin-*` family. The register becomes a read/write noticeboard whose default
mutator is the model, which reads cards natively; a kit that owns a key
treats its card as a serialization — read, destructure against its own
schema, mutate, set again. The tasks and goal kits become *pure preludes over
the family*: no Rust knows what a task or a goal is, and the task kit's
value-threading dies not because state moved out of the card, but because the
card became readable.**

## Context

### What the register is today

`surface `` `pin [key, body] `` decodes through `value_to_pin`
(`exarch/src/bus/card/decode.rs:40`) into `Kind::Pin { key, card }`, which the
viewport keeps in an ordered `key → Card` map (`exarch/src/tui/viewport.rs:401`)
and the session mirrors as `PinDigests` — an `Arc<Mutex<BTreeMap<String,
PinDigest>>>` whose `PinDigest` already holds the **full decoded `Card`**
(`exarch/src/shell_eval.rs:89`), written by `SurfaceApplier::live`
(`exarch/src/fleet/desk.rs:754`). The mirror exists so the boundary nudge can
name what is pinned (`exarch/src/agent.rs:135`,
`exarch/src/agent/nudge.rs:137` via `summary_line`,
`exarch/src/bus/card.rs:229`), and it is already **per agent**: a sub-agent's
register is its own. Nothing reads a pin back.

### The defect: write-only forces threading

Because the register cannot be read, the task kit keeps its list in a binding
and pins a rollup from inside each mutator (`surface-progress`,
`exarch/data/agent.ral:104`). The value and its picture travel by different
mechanisms and separate whenever the model skips the rebind:

- `add-task $exarch-tasks "x"` without `let exarch-tasks =` pins a gauge the
  binding contradicts.
- A block discards its shell-state changes (SPEC §10), so `add-task` inside
  any function, `if` arm, or `within` pins the new gauge and throws the list
  away when the block closes.
- A fork inherits a *copy* of the list and pins to the same key, so parent and
  child overwrite each other's rollup.
- `$exarch-tasks` is a naming convention the checker cannot see.

`set-goal` has none of these, because the goal is write-only into the register
with no value form to drift from. The drafted revision generalized that by
moving the datum into the register and deriving the card from a view; this ADR
generalizes it the other way, by making the card readable so it *is* the
datum.

### Why the card can be the datum

The card grammar is a value grammar. Marks are variants over records —
`` `fields [rows: […]] `` preserves span boundaries per row, `` `measure ``
carries label/value/max, spans carry `role` and `text`
(`exarch/src/bus/card.rs:154`, decode at `decode.rs:67`) — so a kit that
authored a card can destructure it with ordinary ral pattern matching, no
text-scraping. And the other reader needs no schema at all: the model reads
chrome natively; a rendered card *is* legible state to it. Two readers, one
store, and the store is the thing the rail shows.

The bound this sets is real and is accepted deliberately: **only what the card
renders can be read back.** A kit's state must live in the decoder's image,
and its encoding must be injective — two states that render identically are
the same state. `done` and `blocked` must reach distinguishable spans; a field
the rollup never shows does not survive the register. This is the WYSIWYG
invariant, and it is a feature: the register can never again hold a truth the
rail does not show. It has one immediate consequence for the task kit: `tags`
and `notes`, which today's pinned rollup omits, must join the card or leave
the schema — the plan below renders them.

## Decision

### 1. The wire is unchanged; the commands are a `pin-*` family

`` `pin [key, body] `` with a card body, `` `unpin [key] ``, one keyspace, the
protected-key rule for `services` — all exactly as
[[decisions/260622_surface-pins-state|surface-pins-state]] left them. On top
of that wire sit four model-facing commands:

- `pin-set <key> <card>` — overwrite the slot; a ral wrapper over
  `surface `` `pin ``.
- `pin-clear <key>` — empty the slot; a ral wrapper over `surface `` `unpin ``.
- `pin-read <key>` — the stored card, canonically encoded, or `unit`.
- `pin-list` — the occupied keys.

Only the read pair are Rust builtins; the writes are prelude wrappers, since
the surface channel already carries them. The spellings follow the exarch
builtin convention of hyphenated compounds (`view-text`, `edit-replace`,
`skill-list`, `service-handle`, `exarch/src/shell_eval/builtins.rs:915`), and
deliberately avoid bare `pin`/`unpin`/`pinned` as command names — short bare
words are the likeliest to collide with a binary on some user's `PATH`, while
`` `pin `` as a wire variant label is beyond `PATH`'s reach. "set" says a pin
overwrites, "clear" says the slot empties; "add"/"remove" were rejected
because a pin appends nothing and a clear removes no element.

### 2. `pin-read` returns the canonical card

`pin-read` is an enquiry, answered from the agent's own mirror: the stored
card re-encoded as a ral value, or `unit` when the slot is empty. Because the
mirror holds the *decoded* `Card`, the read returns the canonical form, not
the authored bytes: sugar the decoder lifts (a bare string span, a bare mark)
comes back as its record form, and an unknown mark comes back as the plain
text it degraded to. The contract is a host-side encoder inverse to the
decoder on its image: `value_to_card ∘ encode_card = id` on every `Card` the
decoder can produce.

Canonical, not verbatim, is load-bearing. Storing and returning the authored
value would let a kit smuggle state through a payload the decoder degrades —
an unknown mark whose record rides along invisibly — and the register would
again hold something the rail does not show. Reading the canonical card makes
storage and display one thing by construction.

`pin-list` is the same enquiry's other face: the keys currently occupied in
the agent's register, so the model can survey the board before reading a
slot. It returns keys only; the cards stay behind `pin-read`.

Read-after-write inside a single run is sound because `DeskBinding::enquire`
drains queued surface frames before handling the request
(`exarch/src/fleet/desk.rs:817`): a pin written earlier in the script is
already applied when the read is answered.

### 3. The model is the register's default mutator

The whole family is model-facing. The model can list the board, read a slot,
judge it, and set the revision — the register as its noticeboard, with no kit
in the loop. This is what a generic card earns: the model can pin and revise
*anything*, with zero apparatus, because a card needs no schema to be read by
the reader that matters most.

### 4. Tasks and goals are preludes over the family

The host's whole contribution is the four commands; **nothing in Rust knows
what a task or a goal is.** The kits in `exarch/data/agent.ral` are ordinary
prelude code that happens to own a key:

```ral
let add-task = { |desc|
  let ts = decode-tasks !{pin-read "tasks"}   # canonical card → task records
  sync-tasks [...$ts, !{mk-task !{next-id $ts} $desc}]
}
```

`decode-tasks` and `tasks-card` are the kit's serialization pair (specified in
the plan, §D); `sync-tasks` is the one write point, clearing the slot when no
work remains — the list empty or every task done — so a finished board never
lingers to be named by the boundary nudge. All the `map`/`filter`/`fold`
survives; the threading dies. The model-facing surface takes no list —
`add-task "fix the parser"`, `transition 3 `` `doing `` — and
`set-goal`/`clear-goal` collapse to one-line preludes over
`pin-set`/`pin-clear`.

A card under the kit's key that its decoder does not recognize — the model
scribbled on it, exercising §3 — fails the kit call with a didactic `fail`
naming the expected shape. That collision is the price of one keyspace shared
between a schemaless mutator and a schema'd one, named rather than hidden;
the error text is the schema's documentation.

### 5. Residency stays per agent

The register belongs to the agent that pins it, which is what makes
read-mutate-set safe: no other agent holds the key, so there is no lost
update to lock against. Within one agent, concurrent `spawn` bodies can still
race a read against a write; the rule is last-write-wins, stated in the kit
docs — mutate pinned state from the foreground. (`pin-read`/`pin-list` are
enquiries, so inside `spawn { … }` they error like every desk-answered
builtin already does.)

## Implementation plan

Ordered stages; each compiles and passes its own tests before the next
begins. Existing behavior to preserve throughout: the protected-`services`
guard (`reject_protected_pin`, `exarch/src/shell_eval.rs:154`), the nudge
digest (`summary_line` over the same mirror), and the viewport's rendering
path, none of which change.

### A. Encoder: `encode_card`

`exarch/src/bus/card/encode.rs`, sibling to `decode.rs`, exporting
`pub(crate) fn encode_card(card: &Card) -> ral_core::Value`. It emits the
exact shapes `decode.rs` accepts, choosing one canonical spelling wherever
the decoder accepts several:

- The card: `` `card [ <mark>, … ] `` — always the list form, even for one
  mark.
- `Mark::Text` → `` `text [spans: [<span>, …]] ``; a span is
  `[role: "<role>", text: "<text>"]`, with the `role` key **omitted** when
  `None` (`decode_span` reads an absent role as `None`; never emit
  `role: ""`).
- `Mark::Measure` → `` `measure [label: …, value: …] `` plus `max`/`unit`
  keys only when present.
- `Mark::Fields` → `` `fields [rows: [[label: …, value: <v>], …]] `` where
  `<v>` is `` `text [spans: …] `` for `FieldVal::Inline` and
  `` `measure […] `` for `FieldVal::Measure` — always the tagged forms, never
  the bare-string sugar.
- `Mark::Diff` → `` `diff [path: …, hunks: [[start: …, rows: [[tag: "del"|"add"|"context", segs: [[text: …] …]], …]], …]] ``,
  with `emph: true` on a seg only when set.
- `Mark::Raw` → `` `raw [bytes: <Bytes>] ``.
- `Mark::Listing` is host-composed and outside the decoder's image; encode it
  as `` `raw `` of its bytes. The inverse law is stated on the decoder's
  image only, and this is its one deliberate collapse.

Tests, in `encode.rs`: a round-trip covering every mark variant and every
optional field both present and absent (mirror `decodes_every_mark`,
`decode.rs:260`), asserting `value_to_card(&encode_card(&card))`
reconstructs the card structurally; plus a sugar-normalization case —
decode a card written with bare-string spans and a bare mark, encode, decode
again, and assert the second decode equals the first.

### B. Desk: the mirror reaches the enquiry arms

1. Add `pub pins: Option<shell_eval::PinDigests>` to `HostServices`
   (`exarch/src/fleet/desk.rs:29`). Thread it at every construction site
   (`rg 'HostServices \{'`): the agent clones its own `pins` Arc
   (`exarch/src/agent.rs:135`) into the services it assembles; sites with no
   mirror pass `None`.
2. Two arms in `ExarchDesk::handle` (`exarch/src/fleet/desk.rs:237`),
   following the didactic-refusal style of their neighbors:
   - `"pin-read"`: payload `[<key: String>]` via `payload_list`. Look up in
     `pins`; on a hit, `encode_card` the stored card and convert with
     `FOValue::try_from` (a card value is always first-order); on a miss or
     an absent mirror, `FOValue::Unit`.
   - `"pin-list"`: no payload. The mirror's keys as
     `FOValue::List` of strings — `BTreeMap` order, i.e. lexicographic;
     absent mirror → empty list.
3. No drain work: `DeskBinding::enquire` (`desk.rs:817`) already applies
   queued surface frames before handling, which is exactly the
   read-after-write ordering §2 relies on.

Tests, in `desk.rs`'s test module: pin through `SurfaceApplier::live`, then
`handle` a `pin-read` and assert the canonical card comes back; unpin, assert
`Unit`; `pin-list` tracks set and clear; a `pin-read` of `"services"`
answers (reads are not writes, the protected guard does not apply).

### C. Builtins: `pin-read` and `pin-list`

In `exarch/src/shell_eval/builtins/harness.rs`, extending
`HARNESS_BUILTINS_ARR` (`:552`) from 8 to 10 — these are desk-answered, so
they belong with the harness set, not `EXARCH_BUILTINS_ARR`:

- `builtin_pin_read`: the `builtin_unschedule` shape (`:391`) — enquire
  `` `pin-read [<key>] ``, return `Value::from(answer)`.
  Scheme `pin-read :: ∀α. String → F α` (the `from-json` precedent —
  `TyTemplate::Any`, `core/src/typecheck/builtins.rs:391` — trusted, not
  checked; build it like `scheme_reply` at `:545` with the var in the
  return).
- `builtin_pin_list`: the `builtin_schedules` shape (`:380`) — enquire,
  expect `FOValue::List`, error didactically on any other shape.
  Scheme `pin-list :: F [String]` (the `scheme_agents` shape, `:464`).

Doc strings (model-facing; per the standing rule they describe data and
placement, never rendering, and cross-reference nothing):

> `pin-read <key>  — the card currently pinned under KEY on your register,
> as a `` `card `` value you can destructure, or unit if the slot is empty.
> Reads your own register only. Answered only on the run that calls it:
> inside spawn { … } this errors.`

> `pin-list  — the keys currently occupied on your pin register, as
> [String]. Read one back with pin-read. Answered only on the run that calls
> it: inside spawn { … } this errors.`

Tests: the scripted-provider round-trip pattern
(`reply_full_stack_round_trip…`, `harness.rs:1075`) — a script that
`pin-set`s, `pin-read`s in the same run, and replies with the readback;
assert the parent receives the canonical card. A second script reads an
absent key and replies `unit`.

### D. The kit: tasks and goals as preludes

Rewrite the task and goal sections of `exarch/data/agent.ral`. The wrappers
come first:

```ral
let pin-set   = { |key card| surface `pin [key: $key, body: $card] }
let pin-clear = { |key|      surface `unpin [key: $key] }
```

**The tasks card, normatively.** Marks: a `` `measure `` gauge then a
`` `fields `` matrix — `[`measure [label: "tasks", value: <done>,
max: <total>], `fields [rows: …]]`. One row per task:

- `label`: `"#<id>"`.
- `value`: `` `text `` of **exactly four spans**, positionally decoded:
  1. status — text is the status word (`open`/`doing`/`blocked`/`done`),
     which alone carries the datum; role is chrome by the existing mapping
     (`done→ok`, `doing→warn`, `blocked→bad`, `open→muted`).
  2. description — text is `"  " + desc`, the two-space separator stripped
     on decode; desc itself is unconstrained.
  3. tags — `""` when empty, else `" [" + intercalate "," tags + "]"`.
     Injectivity demands tags contain no `,` or `]`: `add-tag` and
     `retag-task` validate and `fail` didactically, the `valid-statuses`
     precedent.
  4. notes — `""` when empty, else `" -- " + notes`; the final span, so
     notes are unconstrained.

This renders `tags` and `notes`, which the old rollup dropped — WYSIWYG
requires it (Context §3).

**The serialization pair and the write point:**

- `tasks-card ts` — the list to the card above.
- `decode-tasks v` — `unit` → `[]`; a card of the exact shape above → the
  task records; anything else →
  `fail "tasks: the card under the 'tasks' pin is not task-shaped — …"`,
  spelling out the expected marks and row form.
- `sync-tasks ts` — `pin-clear "tasks"` when `ts` is empty or every task is
  `` `done ``, else `pin-set "tasks" !{tasks-card $ts}`. Every mutator ends
  in it. This keeps today's `surface-progress` ergonomics: no outstanding
  work, no pin, and the nudge goes quiet on its own. The cost is accepted
  deliberately and documented in the kit: transition-ing the last open task
  to `` `done `` discards the records — a reopen afterwards finds an empty
  register and starts a fresh list (`decode-tasks` of `unit` is `[]`).

**Mutators**, all reading the register, none taking a list:
`add-task <desc>`, `remove-task <id>`, `transition <id> <status>`,
`tag-task <id> <tag>`, `untag-task <id> <tag>`, `note-task <id> <note>`,
`retag-task <id> <tags>`, `clear-tasks` (replacing `empty-tasks`),
`render-tasks`, `find-task <id>`, `by-status <status>`,
`save-tasks <path>` (readback → `to-json`), `load-tasks <path>`
(`from-json` → `sync-tasks`). The pure value helpers — `mk-task`, the
per-field setters, `next-id`, `render-task` — survive unchanged.
`task-to-marks` and `surface-progress` are deleted into `tasks-card`.

**Goal:**

```ral
let set-goal = { |text|
  pin-set "goal" `card [`text [spans: [[role: "strong", text: "goal"]]],
                        `text [spans: [[text: $text]]]]
}
let clear-goal = { pin-clear "goal" }
```

### E. Docs

- `agent_library_docs` (`exarch/src/shell_eval/builtins.rs:74`): every entry
  loses `$exarch-tasks`; `empty-tasks` becomes `clear-tasks`; the ghost
  `status-counts` entry (advertised, never defined) is dropped; `pin-set`
  and `pin-clear` gain entries as library helpers. The builtin index picks
  up `pin-read`/`pin-list` automatically from their entries.
- `exarch/data/tasks.md`: rewritten against the new surface — no bindings,
  no rebinds; the workflow is `add-task`, `transition`, `render-tasks`,
  `pin-read` for anything bespoke.
- Wiki: `map/exarch/builtins.md` (two new builtins, two library wrappers),
  `map/exarch/cards.md` (the encoder and the canonical-form rule),
  `design/pins.md` (rewritten against this ADR),
  `map/exarch/shell-eval.md` if it names `PinDigest`'s shape.

### F. Whole-system tests

Beyond the per-stage tests above:

- **Kit round-trip** (wherever `agent.ral` is exercised today —
  `rg 'add-task' exarch` for the harness): `add-task`, `transition`,
  `tag-task`, then `render-tasks` and a direct `pin-read "tasks"`; assert
  the decoded list holds every field, including tags and notes.
- **Block survival**: `add-task` inside a function body, then read the list
  from the top level — the defect this ADR exists to close.
- **Isolation**: a sub-agent pins `"tasks"`; the parent's `pin-read` still
  answers the parent's card.
- **Protection**: a model `pin-set "services" …` is refused (existing
  guard), and `pin-read "services"` still answers.
- **All-done clears**: `transition` the last open task to `` `done `` —
  `pin-read "tasks"` answers `unit`, and a subsequent `add-task` starts a
  fresh list at id 1.
- **Schema collision**: `pin-set "tasks"` a plain text card, then
  `add-task` — the didactic `fail` fires, naming the expected shape.

## Alternatives considered

- **State-carrying pins with typed register handles** — `` `pin `` carries a
  first-order datum, a `[key, view]` record renders it host-side at pin
  time, and the read verb returns the handle's `α`. This ADR was drafted in
  that shape, and it is the strongest alternative: the checker relates state
  to view, and the read is statically typed. Rejected because the handle is
  mandatory — there is no viewless pin, so the generic act, *the model
  pinning some card*, pays for machinery only kit plumbing uses. It also
  grows three typed builtins, a paired `(state, card)` register, and in-run
  view application, where this ADR grows one enquiry class and an encoder.
  The type dividend returns if kit-level runtime decode failures ever hurt
  in practice; the drift defect it targeted is closed here by identity
  instead — there is only one thing to drift.
- **Verbatim read-back: store the authored value beside the decoded card.**
  Rejected. It spares the encoder and returns byte-faithful values, but an
  authored value can carry what the decoder degrades, so the register would
  hold state the rail does not display — the hidden-truth defect in a new
  coat. Canonical read-back forecloses it structurally.
- **A `state` slot beside the card, written per pin.** Rejected: a kit could
  author a card that misdescribes its datum, and nothing relates the two.
  The WYSIWYG identity is strictly stronger.
- **Typed handles as an optional layer over bare pins.** Rejected: two pin
  forms, two register semantics, every reader handling both. The didactic
  decode error is the cheaper answer to the same risk.
- **Bare verbs — `pin`/`unpin`/`pinned`/`recall` — as the command names.**
  Rejected on two grounds. `recall` squats on fleet vocabulary (recalling a
  sub-agent is a plausible future verb) and says nothing about pins. The
  bare pin family reads well, but short bare words are the likeliest `PATH`
  collisions, and exarch's builtin register is uniformly hyphenated
  compounds; `pin-*` keeps the family greppable and self-locating.
- **`pin-add`/`pin-remove` as the write spellings.** Rejected: "add" implies
  appending to a collection when a pin overwrites one slot, and "remove"
  implies deleting an element when a clear empties the slot.
- **Host-owned task state: `add-task`/`transition` as Rust builtins.**
  Rejected: it buys atomic mutation, which per-agent residency already
  gives, and pays a Rust task module serving exactly one kit — the opposite
  of §4's "kits are preludes". The list logic is better ral than Rust.

## Consequences

- The register is a read/write noticeboard: the model can list the board,
  read any pin, and revise it; a kit can round-trip its own. `pin-read` and
  `pin-list` are the only new builtins; `pin-set` and `pin-clear` are ral
  wrappers over the existing wire.
- Tasks and goals are pure preludes over the family; the host's vocabulary
  ends at the four commands, and any future kit gets the same substrate for
  free.
- Storage and display are one value. Pin and picture cannot drift because
  they are identical, and the register can never hold what the rail does not
  show — which obliges the task card to start rendering tags and notes.
- Task state survives blocks, `within`, function bodies, and forks — the
  read comes from the register, so SPEC §10's discard rule no longer eats
  updates, and `$exarch-tasks` threading is deleted rather than repaired.
- The typing is dynamic where the drafted design was static: `pin-read`
  types as `∀α. String → F α` on the `from-json` precedent, and a kit's
  decode failures surface at runtime as didactic `fail`s, not at check
  time. Accepted as the cost of a handle-free generic pin.
- Expressiveness is bounded by the card vocabulary: a kit needing state the
  rail should not show must keep it out of the register.
- No outstanding work means no pin, as today: the slot clears when the list
  empties or every task is done, so the boundary nudge stops naming a
  finished board. The accepted cost, named: since the register is now the
  store, marking the last open task done discards the task records, and a
  later reopen starts a fresh list.
- exarch grows one enquiry class (two arms), one encoder, and two builtins.
  No core change; no new wire format; no new concurrency invariant.

## Open questions

- **Should a child's final register reach its parent?** A returning agent's
  pinned cards die with it. Surfacing them is plausibly the *matrix's* job;
  left out of scope.
- **Snapshot persistence.** Still ephemeral, and now readable: a register
  snapshot would let `/clear` and replay restore a task list. Deferred, as
  before, but the cost of deferring has gone up.
- **Reading a foreign key.** `pin-read` reads the agent's own register only.
  Cross-agent reads, if ever wanted, want a distinct verb and an authority
  rule, not a widened `pin-read`.

## See also

[[decisions/260622_surface-pins-state|surface-pins-state]] (the register this
completes — wire format and taxonomy kept, write-only-ness retired),
[[design/pins.md|pins]] (the design page to rewrite against this),
[[decisions/260622_surface-carries-control|surface-carries-control]] (dispatch
by class — `pin-read` joins the enquiry desk the same way),
[[decisions/260619_surface-carries-documents|surface-carries-documents]] (the
card vocabulary that now doubles as the register's storage grammar),
[[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]]
(encode-don't-stream, which canonical read-back extends to reads),
[[map/exarch/cards|cards]], [[map/exarch/shell-eval|shell-eval]],
[[map/exarch/agent|agent]] (the nudge that names pins from the same cards
`pin-read` returns), and `exarch/data/agent.ral` (the client that loses its
threading).
