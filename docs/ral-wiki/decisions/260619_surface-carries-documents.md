---
status: accepted
---

# `surface` carries a render document, not a closed tag set

**The `surface` builtin should hand exarch a *render document* — an ordered stack
of Bertin *marks* the kit composes entirely in ral — which one generic
interpreter binds to visual variables, instead of a closed set of tagged variants
(`` `patch ``/`` `wrote ``/`` `task ``/`` `meter ``) that exarch decodes into a
closed `Kind` enum with a bespoke renderer each.** The *set of cards* becomes open
(compose marks in ral, zero Rust per card — so the kit can surface a captured file
write, a build summary, a test matrix, anything); the *set of marks* stays closed
and small, so the renderer is total and reflow, disclosure, aggregation, and the
structured `events.jsonl` log all keep working. The discipline is the one
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] applied
to the frontend's own *chrome* — encode each datum in the Bertin variable that
fits its level of measurement — now extended to kit-authored *content*: the kit
declares data and whether it is *ordered* or *nominal*; exarch owns the binding to
size/value/grain (magnitude) or hue/shape (identity). Core is untouched — it
already carries raw `Value` ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]).

This change has landed: the `Card`/`Mark` vocabulary and `value_to_card`
decoder live in `exarch/src/card.rs`, the one generic `render_card`
interpreter and the role binding table in `tui/line.rs`, `BlockKind::Card`
with derived disclosure and single-`diff` aggregation in `tui/block.rs` and
`tui.rs`, and the kit emits cards (`patch-card`/`wrote-card` in `agent.ral`,
`task-card`/`meter-card` in `kit/tasks.ral`); `Kind::{Patch,Wrote,Task,Meter}`
and `TaskStatus` are gone.

## The diagnosis

`surface <value>` (`core/src/builtins/misc.rs:530`) forwards one `Value` to a
turn-local `EventSink` (`core/src/types/shell/mod.rs:156`); core knows nothing of
what the value *means*, and a detached worker buffers the `Value`s and replays
them on `await` (`core/src/types/value.rs:271`). **Core is not the hack.** The
hack is entirely in exarch, and it has two faces:

- **A closed tag set, decoded by hand.** `value_to_kind`
  (`exarch/src/shell_eval.rs:226`) matches a fixed vocabulary — `` `patch ``,
  `` `wrote ``, `` `task ``, `` `meter `` — into the closed `Kind` enum
  (`exarch/src/bus.rs:138`, variants at `:201`–`:228`), plus a closed `TaskStatus`
  (`exarch/src/bus.rs:106`). Anything else is dropped.
- **A bespoke renderer per tag, in five places.** Each `Kind` has a hand-written
  TUI builder (`exarch/src/tui/line.rs`: `patch:409`, `wrote:556`, `task:585`,
  `meter:609`), a TUI dispatch arm (`exarch/src/tui.rs:610`–`629`), a `Block`
  variant (`exarch/src/tui/block.rs`), a JSON arm (`exarch/src/headless.rs:137`,
  `:148`, `:156`), and an stderr arm (`headless.rs:276`+).

So *one* new surfaceable thing is a cross-cutting change across two crates and
five sites. That is the non-extensibility. And the channel is not even uniform:
`task`/`meter`/`wrote` land as pre-rendered `Chrome` blocks
(`block.rs:142`, inert, not dialable) — they are *already* "lines exarch just
prints" — while `patch` alone is a structured `Block::patch{path,hunks}`
(`block.rs:139`) the TUI re-renders from data at the user's disclosure level
(`block.rs:227` `dial`, `:253` `lines`), aggregates across consecutive emits
(`tui.rs:660` `absorb_patch`), and whose magnitude feeds the size-bar and the
agent matrix. The "hack" is that one shape earns rich, data-driven treatment,
three are dumb chrome, and a hardcoded enum sorts them.

## The decision

`surface` carries a **card**: an ordered stack of **marks** on the plane (one
scrollback `Block`), composed in ral. exarch decodes it once into a closed Rust
`Card`/`Mark` model and renders it through *one* interpreter. Adding a card is
composition in ral; it touches no Rust.

### The Bertin discipline — the heart of it

The kit declares **data and its level of measurement, never its appearance.**
Bertin's visual variables split by what they can faithfully carry:

| level of measurement | Bertin variables | what marks bind it |
|----------------------|------------------|--------------------|
| **quantitative / ordered** (magnitude, comparable) | position, **size**, **value** (lightness), **grain** | `measure`, `diff` |
| **nominal / selective** (identity, not ranked) | **hue**, **shape**, orientation, texture | a `text` span's *role* |
| none (plain ink) | — | default |

A mark says *"this is a magnitude"* or *"this is a nominal identity"*; the
renderer holds the **one** binding table — quantitative → size + value + grain,
nominal → hue + shape — and the single palette. The kit therefore *cannot* put
magnitude on hue (the precise error
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] rejects
in its "Alternatives considered": *"Encode magnitude in hue → rejected: hue is
unordered."*). The encoding is correct by construction. The palette lives once in
exarch (themeable); the TUI gets styled marks and `events.jsonl` gets the data
plus its level — both from one description, never two.

### The marks — closed, small, composable

A **`card`** is a `List` of marks rendered top-to-bottom on the plane, carrying
(free, from that ADR's Phase 0) the producing agent's hue on the marginal rail.
Four encoded marks, one escape hatch:

1. **`text`** — the *qualitative* mark: a run of spans. A span may carry a
   *nominal role* mapped to hue/shape — `path`, `code`, `ok`, `warn`, `bad`,
   `muted`, `strong` — and never a magnitude. A heading is just a `strong` span.

2. **`measure`** — the *quantitative* mark: `[label, value, max?, unit?]`,
   rendered with the two ordered variables size (a `log2` bar) and value
   (lightness ramp). Subsumes `meter` *and* every header size-bar
   (`line.rs:127` `size_bar`, `:609` `meter` collapse into one).

3. **`fields`** — the *matrix* mark: an aligned `(label, value)` list in one
   shared label column — Bertin's selective alignment, his reorderable table in
   miniature (this is exactly `provider_error`'s `Field`/`push_field`
   discipline, `line.rs:915`, generalised). Values nest marks (`text`/`measure`).
   Subsumes `task` (status is a field whose value is a nominally-roled `text`).

4. **`diff`** — the *dense composite* mark: `[path, hunks]`. The one mark that
   binds four variables the renderer already computes — **size** (magnitude bar),
   **grain** (add/del texture, `line.rs:151`), **value** (rail lightness),
   **shape** (`▎`). It stays first-class precisely because it is the densest
   Bertin object; it subsumes `patch`, *including* cross-emit aggregation and
   graded disclosure.

5. **`raw`** — *un-encoded ink*: pre-formed bytes appended verbatim (captured
   tool output, a file write, anything outside the grammar). Honest about being
   outside Bertin's variables — it is an image, not an encoding. The "just bytes"
   surfacing the original instinct reached for is the degenerate single-`raw`
   card; it is kept, as one mark among five, not as the whole channel.

Composability is one rule applied at three scales: **the plane stacks marks**
(`card`), **`fields` nests marks** in its value column, **`text` nests roles** in
its spans. Nothing else.

### Wire shape and the one decoder

The document is an ordinary `Value`: `Variant{label:"card", payload: List<mark>}`;
each mark is `Variant{label, payload: Map}`. The kit writes:

```
surface `card [
  `text   [spans: [[role: "strong", text: "edited "], [role: "path", text: $path]]],
  `diff   [path: $path, hunks: $hunks],
  `fields [rows: [["tests", "42 passed"], ["status", `text [spans: [[role: "ok", text: "done"]]]]]],
  `measure[label: "crates", value: 7, max: 12],
  `raw    [bytes: $captured],
]
```

`value_to_card` replaces `value_to_kind` as the **one** decoder. Crucially it does
*not* grow an arm per card — a card is a composition of the five marks, so the
decoder is fixed-size forever. An unknown mark label or role renders plain (a
softer degradation than today's silent drop, because a card is a deliberate
user-facing act, not a sentinel that might be malformed).

### What this deletes, generalises, and leaves alone

*All exarch; core unchanged.*

- `Kind::{Patch,Wrote,Task,Meter}` (four variants) → `Kind::Card(Card)` (one).
  `TaskStatus` + `parse`/`tag` (`bus.rs:106`) is **deleted** — the closed
  status set was itself hardcoded vocabulary; the kit now picks which status maps
  to which nominal role (`ok`/`warn`/`bad`/`muted`), which is where domain
  knowledge belongs.
- `line::{wrote,task,meter}` stop being bespoke; their pixels are recovered by
  composing `text` + `fields` + `measure`. `line::patch` (`:409`) *becomes* the
  `diff` renderer; `provider_error`'s `Field`/`push_field` (`:755`/`:915`)
  *become* the `fields` renderer (shared — `provider_error` itself is then one
  internal caller of it, killing a WET seam); `size_bar`/`meter` *become* the
  `measure` renderer.
- `headless.rs`'s four JSON arms → one `card` serializer (the mark tree, so the
  machine log stays structured); the stderr condenser walks marks generically.
- `Block`: the `Patch`/`Chrome`-for-these variants → `BlockKind::Card{marks}`.
  Disclosure is **derived** — a card containing a `diff` is dialable (L1 header /
  L3 full), a card of only `text`/`fields`/`measure`/`raw` is chrome-level
  (L3-only), reproducing today's split without naming it. Aggregation keys on a
  single-`diff` card + path, mirroring `absorb_patch` (`tui.rs:660`).
- **Core: nothing.** `surface`, `EventSink`, the `Arc<dyn EventSink>` carrier,
  and detached-worker buffering/replay are untouched. Cards are `Value`s, so
  deferred surface replay works for free — a `spawn { … }` that surfaces a card
  replays it on `await` exactly as a patch does today.

### Kit side (ral)

The agent/tasks libraries gain small constructors so call sites stay DRY and the
mark grammar lives in one ral place: `patch-card`, `wrote-card`, `task-card`,
`meter-card`, each building a `` `card `` from marks. `agent.ral:128`'s
`surface `patch [...]` becomes `surface (patch-card $path $hunks)`. The system
prompt (`exarch/data/ral.md`, `exarch/data/system.md`) teaches the five marks and
the role list, so the model can compose *new* cards itself — the point of the
whole change.

## Why this shape

- **One discipline, chrome and content.** `tui-transcript-as-graphic` Bertin-
  encoded the rail, the matrix, the headers — the frontend's *own* ink. This
  encodes the *kit's* ink with the same variable bindings, so a surfaced card and
  the chrome around it speak one visual language instead of two.
- **Open cards, closed marks — the only split that is extensible *and* total.**
  A closed *card* set is the present hack; an open *mark* set would make the
  renderer partial (it could meet a mark it cannot draw). Open compositions over
  a closed, small mark vocabulary is both.
- **Correct by construction.** Because the kit names *level of measurement*, not
  RGB, magnitude can never land on hue and identity can never masquerade as
  size — the renderer binds, and there is exactly one binding table to get right.
- **Two consumers, one description.** `events.jsonl` is a machine record a harness
  parses (`headless.rs`); the mark tree serialises structurally, while the TUI
  styles the same tree. Raw bytes would have made the card opaque to the log;
  here only a `raw` mark is opaque, and honestly so.
- **`raw` keeps the original instinct.** "Bytes exarch prints" is not lost — it
  is one mark, for exactly the open-ended case (captured output, file writes)
  where no encoding applies.

## Implementation plan

Five landable parcels; each compiles and tests on its own. Parcels 0–1 add the
model and renderer beside the existing `Kind`s (no behaviour change); parcel 2
flips the dispatch and deletes the old variants; parcels 3–4 move the kit and pin
the contracts.

```
0  Vocabulary     Card/Mark enums, value_to_card, JSON serde, Kind::Card
1  Renderer       one render_card(&Card, width) -> Vec<Line>; the binding table + palette
2  Block cutover  BlockKind::Card; derived disclosure + aggregation; delete 4 Kinds, TaskStatus
3  Kit migration  ral constructors; port agent.ral + tasks kit; system-prompt vocabulary
4  Tests          decode totality, render parity, aggregation, disclosure, JSON, raw, replay
```

### Parcel 0 — the vocabulary

- In `exarch/src/event.rs` (or a new `exarch/src/card.rs`): `Card(Vec<Mark>)`;
  `Mark = Text(Vec<Span>) | Measure{label,value,max,unit} | Fields(Vec<(String,
  FieldVal)>) | Diff{path,hunks:Vec<Hunk>} | Raw(Vec<u8>)`, where
  `Span{role: Option<Role>, text: String}`, `FieldVal = Inline(Vec<Span>) |
  Measure(...)`, and `Role` is the closed nominal set (`Path|Code|Ok|Warn|Bad|
  Muted|Strong`). `Hunk` is reused as-is (`bus.rs:238`).
- `value_to_card(&RalValue) -> Option<Card>`: the one decoder, reading marks off
  the runtime value the way `value_to_kind` reads fields today
  (`shell_eval.rs:234`–`251` helpers move over). Unknown mark/role → a plain
  `text` fallback, not a drop.
- `Kind::Card(Card)` added to the bus (`bus.rs:138`); `AgentSink::emit`
  (`shell_eval.rs:59`) calls `value_to_card` and emits `Kind::Card`. The four old
  Kinds stay this parcel so nothing breaks.
- `serde::Serialize` on `Card`/`Mark`/`Span` for `events.jsonl`.

### Parcel 1 — the generic renderer

- `tui/line.rs` gains `render_card(card: &Card, width: u16) -> Vec<Line>` and the
  **binding table**: `role_style(Role) -> Style` (the one place hue/shape live)
  and the quantitative encoders reused — `measure` calls a generalised
  `size_bar`, `diff` calls the existing `patch` body, `fields` calls a
  generalised `push_field`. Goal: byte-for-byte parity with the four current
  cards, proven by a parity test that builds the equivalent `Card` and diffs the
  `plain()` rows against `patch`/`wrote`/`task`/`meter`.
- `provider_error` (`line.rs:755`) is refactored to build a `Vec<Field>` and call
  the shared `fields` renderer — collapsing the duplicate alignment logic.

### Parcel 2 — Block cutover

- `BlockKind::Card{marks: Card}` (replacing the `Patch` and the task/meter/wrote
  `Chrome` constructions); `Block::dialable`/`magnitude`/`render` dispatch on
  whether the card holds a `diff`. `absorb_patch`/`patch_buf` (`tui.rs:337`,
  `:660`) generalise to "consecutive single-`diff` same-path cards merge."
- Delete `Kind::{Patch,Wrote,Task,Meter}`, `TaskStatus`, `line::{wrote,task,
  meter}`, the four `tui.rs` dispatch arms, the four `headless.rs` JSON arms (→
  one `card` arm) and stderr arms (→ one generic mark walk).

### Parcel 3 — kit migration

- ral constructors in the agent/tasks libraries; port `agent.ral:128` and the
  tasks kit's task/meter emits; rewrite the surface section of
  `exarch/data/ral.md` + `exarch/data/system.md` to teach the marks and roles.

### Parcel 4 — tests

Listed under **Test plan**.

## Alternatives considered

- **Pure bytes the host prints (the original instinct).** Rejected as the *whole*
  channel: opaque to `events.jsonl`, kills width-reflow / click-to-disclose /
  patch aggregation / matrix magnitude, and forces the kit to pick one rendering
  and to learn ANSI. Kept, correctly scoped, as the `raw` mark — the one place no
  encoding applies.
- **Keep the typed Kinds, add one generic "card" beside them (hybrid).**
  Rejected: it preserves the typed-vs-generic split that *is* the hack, and
  leaves two codepaths to maintain.
- **Kit emits concrete styles (RGB / `Style`) rather than nominal roles.**
  Rejected on Bertin grounds: it lets the kit bind magnitude to hue, scatters the
  palette across kit code, breaks theming, and makes the JSON consumer carry
  presentation. The level-of-measurement indirection — kit names the data, host
  names the variable — is the entire point.
- **A core `CardEvent` taxonomy decoded in core.** Rejected: it moves the mark
  vocabulary into core, reversing `run-turn-host-loop`'s deliberate choice that
  core carry only raw `Value` and name no rail/card vocabulary.
- **An open mark set (kit can define new mark kinds).** Rejected: it makes the
  renderer partial — a mark it cannot draw has no faithful fallback. Open
  *compositions* over closed marks gives extensibility without partiality.

## Consequences

- A new surfaceable thing is **zero Rust**: compose marks in ral, optionally add
  one constructor. The model can invent cards from the system-prompt vocabulary.
- One renderer, one palette, one serializer, one decoder — and `provider_error`
  folds into the shared `fields` path. Net Rust *shrinks*.
- The encoding is correct by construction; chrome and content share the Bertin
  bindings, so the transcript reads as one graphic.
- `events.jsonl` stays structured (the mark tree); only a `raw` mark is opaque,
  and that is honest.
- Core is untouched; detached-worker card replay is free.
- Cost, stated plainly: one upfront vocabulary + generic renderer, against which
  four bespoke renderers and a status enum retire. `TaskStatus`'s closed-set
  safety moves into the kit — acceptable, since the kit already owns the tasks
  domain, and an unknown role degrades to plain ink rather than a dropped event.

## Test plan

- **Decode totality.** `value_to_card` on every mark, on nested `fields`/`text`,
  and on malformed/unknown marks (→ plain `text`, never panic, never silent drop).
- **Render parity.** For each of the four legacy cards, build the equivalent
  `Card` and assert its `plain()` rows equal today's `patch`/`wrote`/`task`/
  `meter` output — the cutover changes nothing visible.
- **Aggregation & disclosure.** Consecutive single-`diff` same-path cards merge
  into one block (port of the `absorb_patch` tests, `tui.rs:2662`); a `diff` card
  dials L1↔L3 (port of `viewport.rs:658`); a `fields`/`measure` card is L3-only.
- **Bertin binding.** A larger `measure`/`diff` renders a brighter value and a
  fuller size bar than a smaller one; a nominal role renders a hue, never a size —
  i.e. magnitude and identity cannot be transposed.
- **JSON shape.** A `card` serialises to a structured mark tree in `events.jsonl`;
  a `raw` mark serialises its bytes; round-trips through `serde`.
- **Detached replay.** `spawn { … surface `card … }` then `await $h` replays the
  card to the foreground rail; an un-awaited worker emits none — the byte-replay
  rule, unchanged.

## See also

[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] (the
chrome-side Bertin encoding this extends to kit content — same variables, same
bindings), [[decisions/260618_run-turn-host-loop|run-turn-host-loop]] (core
carries raw `Value`; the surface is presentation, not liveness, so this change is
exarch-only), [[map/exarch/cards|cards]] (the as-built map of this subsystem),
[[map/exarch/frontend|frontend]] (the `Block`/`Viewport`/`line` arm this
re-grounds), [[map/exarch|map: exarch]], and Jacques Bertin,
*Sémiologie graphique* (1967): the plane and its retinal variables, ordered
versus selective, and the rule that the variable must match the datum's level of
measurement.
