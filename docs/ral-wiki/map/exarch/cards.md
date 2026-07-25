---
generated_at_commit: f7cf93a
generated_at_date: 2026-07-25
covers_paths: [exarch/src/bus/card.rs, exarch/src/tui/line.rs, exarch/src/tui/palette.rs, exarch/src/tui/block.rs, exarch/src/tui/surface.rs, exarch/src/tui/viewport.rs, exarch/data/agent.ral]
---

# Map: exarch / cards

The `surface` builtin carries a **render document** — a `` `card ``, an ordered
stack of Bertin *marks* a kit composes entirely in ral. exarch decodes it once
into a closed Rust model and draws it through one generic interpreter. The *set
of cards* is open (compose marks, zero Rust per card — a surfaced file write, a
build summary, a test matrix, anything); the *set of marks* stays closed and
small, so the renderer is total and width-reflow, click-to-disclose, patch
aggregation, and the structured `transcript.jsonl` log all keep working. This is
the [[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]]
discipline extended from the frontend's own chrome to the kit's *content*: the
kit declares **data and its level of measurement**, never appearance; exarch
owns the binding to visual variables. See
[[decisions/260619_surface-carries-documents|surface-carries-documents]].

## The marks

A `` `card `` is a `List` of marks rendered top-to-bottom on one scrollback
[[map/exarch/frontend|block]]. Six marks, closed (`exarch/src/bus/card.rs`):

- **`text`** — the qualitative mark: a run of spans. A span carries an optional
  nominal **`Role`** (`path`/`code`/`ok`/`warn`/`bad`/`muted`/`strong`) mapped to
  a hue, never a magnitude. A heading is a `strong` span.
- **`measure`** `[label, value, max?, unit?]` — the quantitative mark, two
  ordered variables (size + value/lightness). Bounded (`max` present) → a
  proportional fill bar; unbounded → a `log2` size bar.
- **`fields`** — the matrix mark: an aligned `(label, value)` table in one shared
  label column (Bertin's selective alignment). A value nests a `text` or
  `measure` mark.
- **`diff`** `[path, hunks]` — the dense composite, binding four variables exarch
  already computes: size (magnitude bar), grain (add/del texture), value (rail
  lightness), shape (`▎`). The one mark that earns graded disclosure and
  cross-emit aggregation.
- **`listing`** — a numbered source listing: the head of a written file,
  gutter-numbered and syntax-lit but one-sided (no `+`/`-`) — the write card's
  preview of *what was written* when there is no old side to diff against.
- **`raw`** — un-encoded ink: pre-formed bytes appended verbatim, for output
  outside the grammar. Honest about being an image, not an encoding.

Composability is one rule at three scales: the plane stacks marks (`card`),
`fields` nests marks in its value column, `text` nests roles in its spans.

## Decode — `value_to_card`

`value_to_card` (`bus/card.rs`) is the card decoder, reading marks off the runtime
`Value`; `decode_surface` ([[map/exarch/shell-eval|shell-eval]]) tries the pin,
io, and notice shapes first ([[map/exarch/io-surface|io-surface]]). The wire
shape is `Variant{label:"card", payload: List<mark>}`; each mark is
`Variant{label, payload: Map}`. A bare known mark surfaced unwrapped
(`` `diff [...] ``) is lifted into a one-mark card for convenience; any other
top-level value returns `None` and is dropped. Decoding never fails *within* a
recognised card: an unknown mark label or role degrades to plain `text`,
because a card is a deliberate user-facing act, not a sentinel that might be
malformed. The `diff` mark reads a `path` and a `hunks` list — each hunk a
`start` line and a `rows` list of `{tag, text}` records; a missing `hunks`
lifts to empty so a bare diff still renders. Detached workers buffer their
`surface` calls and replay them on `await`, so a card replays for free.

## Host-composed one-liners — `done`, notices

Some cards are composed in Rust, never decoded from a kit-authored `` `card ``.
`done_card` renders a detached worker's completion outcome — the `` `done ``
event core appends to every deferred surface batch, decoded by `value_to_done`
into a `Kind::Done { outcome, card }` — as one line: an outcome span roled by
how it settled (`ok`/`bad`), plus a plain gloss naming it a background block.
`notice_card` is its sibling for core's ready-boundary housekeeping
(`value_to_notice` → `Kind::Notice { notice, card }`): a `Notice::Reap` renders
through `reap_card` as a `warn` span plus the worker's `cmd` and which lease
fired ("idle 1h unobserved" / "24h backstop"), with prune and large-binding
notices rendered by its per-kind siblings
([[decisions/260706_enquiry-channel|enquiry-channel]]). All are fixed-position
value marks, never an animation, and all stay inside the existing `text` mark
vocabulary, so none widens the closed mark set above. `services_pin_card` is
the host-authored protected `services` pin ([[map/exarch/agent|agent]]).

A notice's raw facts ride beside its rendered card on the bus's
`Kind::Notice`, the same pairing `Kind::Io` uses for a structural I/O event:
the raw fact reaches `transcript.jsonl`, the card is a rendering and does not
([[map/exarch/agent|agent]]).

## Render — one interpreter, one binding table

`render_card(&Card, level)` (`tui/line.rs`) walks the marks; `role_style(Role)`
(over the palette constants in `tui/palette.rs`) is the **single place hue
lives** for kit content, so the kit can name a role but never a colour, and
magnitude can never land on hue — the encoding is correct by construction. The quantitative encoders are reused, not duplicated: `measure`
calls the generalised `size_bar`/`progress_bar` through `measure_value_spans`,
`diff` calls the patch body (`diff_body`), and `fields` plus `provider_error`
both feed the shared `render_field_rows`/`push_field` matrix primitive — so
`provider_error` is one internal caller of the `fields` path, not a duplicate
label-column. The diff header label reads `diff`.

`render_card` opens with the single leading blank every block wears; the
data-encoding rail span is prepended by the [[map/exarch/frontend|block]] to the
first content row. A diff-less card the model *surfaced* deliberately renders
through `render_card_framed` instead — an indented box with its heading lifted
into the top rule, no rail glyph (the frame is its mark, see
[[map/exarch/frontend|frontend]]).

## Block — derived disclosure and aggregation

`BlockKind::Card{card, origin}` (`tui/block.rs`) carries the render document and
a `CardOrigin` (`Observation`/`Write`/`Surfaced`) telling the coalescing
projection whether the card is a foldable effect or a barrier. Disclosure is
**derived**, not named: a card holding a `diff` is dialable (`dialable()` →
`Card::has_diff()`) and renders L1 header / L2 first-hunk / L3 full; a card of
only `text`/`fields`/`measure`/`listing`/`raw` is chrome-level (L3-only, inert). The rail
shape is `▎` for a file mutation — a diff card or a write card alike — and
none for a framed surfaced card; an observation card folds into its ral group
rather than carrying its own rail. `magnitude()` is the summed diff
magnitude, feeding the rail's value-step and the agent matrix's size readout;
`lines_changed()` exposes the same diff total as the matrix's write footprint,
distinct from prose volume.

A single-`diff` card joins the patch-grouping buffer in `tui/surface.rs`
(`Card::into_single_diff` → `SurfaceBuffer::absorb_patch`): consecutive
same-`(id, path)` diff cards merge their hunks into one `diff <path>` block, the
way a unified diff presents one file. Every richer card is its own block, pushed
via `Viewport::push_card` (`tui/viewport.rs`); grouped observation effects land
via `push_observation_card`, a write card via `push_write_card`.

## Machine log

`agent/transcript.rs` (the session-owned recorder, fed at the emit seam) serialises a
card to a structured mark tree in `transcript.jsonl` (one `card` arm, the whole
tree via serde); only a `raw` mark is opaque, and honestly so. The headless
stderr condenser (`card_stderr`, `headless.rs`) walks marks generically.

## Kit side

The kit declares data and its level of measurement; the host binds it to visual
variables, and with no host `surface` is the identity, so a kit stays runnable in
a bare ral REPL. The tasks library holds the small constructors so the mark
grammar lives in one ral place: `task-to-marks`/`surface-progress` in
`exarch/data/agent.ral` (the tasks section; the kit owns the status→role
mapping, since the host knows only the closed role set), surfaced per
transition through `transition`/`add-task` onto the pinned `tasks` card.

The agent library's surfacing constructors are gone: `view-text`, `grep-files`,
and `edit-hash`/`edit-replace` are Rust host builtins
([[map/exarch/io-surface|io-surface]]), their file I/O sunk below the redirect
frame so each is one logical surface. An edit's whole-file diff is built at the
write card from the committed write event's old/new snapshots (one canonical
original-vs-final diff grouped into hunks by `similar`); the read redirect and
exec cards are likewise composed from core's I/O events. `agent.ral` now carries
only the `view-text-around` reader, the tasks kit, and the goal pins
([[map/exarch/builtins|builtins]]).

One ral constraint shapes the wire format: lists and records are statically
**homogeneous**. Heterogeneous *variant* lists are fine (ral unifies them into a
sum, so `` `card [text, measure, diff] `` typechecks), but within a homogeneous
list every record needs the same fields — so every `text` span carries a `role`
(use `""` for plain ink), and `fields` rows are records `[label:, value:]`, not
positional `[label, value]` pairs (which would force label and value to one
type). `exarch/data/surface.md` teaches the model the marks, the roles, and
this constraint.

## See also

[[decisions/260619_surface-carries-documents|surface-carries-documents]] (the
decision), [[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]
(the chrome-side Bertin encoding this extends), [[map/exarch/frontend|frontend]]
(the bus / block / line arm this re-grounds), [[map/exarch/shell-eval|shell-eval]]
(the host sink that decodes the card), [[map/exarch/io-surface|io-surface]] (the
core I/O events the sibling decoder turns into cards), [[map/exarch|map: exarch]].
