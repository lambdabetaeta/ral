---
generated_at_commit: 462afa4
generated_at_date: 2026-06-19
covers_paths: [exarch/src/card.rs, exarch/src/tui/line.rs, exarch/src/tui/block.rs, exarch/src/tui/viewport.rs, exarch/data/agent.ral, kit/tasks.ral]
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
[[map/exarch/frontend|block]]. Five marks, closed (`exarch/src/card.rs`):

- **`text`** — the qualitative mark: a run of spans. A span carries an optional
  nominal **`Role`** (`path`/`code`/`ok`/`warn`/`bad`/`muted`/`strong`) mapped to
  a hue, never a magnitude. A heading is a `strong` span.
- **`measure`** `[label, value, max?, unit?]` — the quantitative mark, two
  ordered variables (size + value/lightness). Bounded (`max` present) → a
  proportional fill bar (the old progress meter); unbounded → a `log2` size bar
  (the old header bar).
- **`fields`** — the matrix mark: an aligned `(label, value)` table in one shared
  label column (Bertin's selective alignment). A value nests a `text` or
  `measure` mark.
- **`diff`** `[path, hunks]` — the dense composite, binding four variables exarch
  already computes: size (magnitude bar), grain (add/del texture), value (rail
  lightness), shape (`▎`). The one mark that earns graded disclosure and
  cross-emit aggregation.
- **`raw`** — un-encoded ink: pre-formed bytes appended verbatim, for output
  outside the grammar. Honest about being an image, not an encoding.

Composability is one rule at three scales: the plane stacks marks (`card`),
`fields` nests marks in its value column, `text` nests roles in its spans.

## Decode — `value_to_card`

`value_to_card` (`card.rs`) is the card decoder, reading marks off the runtime
`Value` the way the old `value_to_kind` read fields (a sibling `value_to_io`
decodes core's I/O events — [[map/exarch/io-surface|io-surface]]). The wire shape is
`Variant{label:"card", payload: List<mark>}`; each mark is `Variant{label,
payload: Map}`. A bare known mark surfaced unwrapped (`` `diff [...] ``) is
lifted into a one-mark card for convenience; any other top-level value is
dropped, exactly as the old decoder dropped an unrecognised variant. Decoding
never fails *within* a recognised card: an unknown mark label or role degrades
to plain `text`, because a card is a deliberate user-facing act, not a sentinel
that might be malformed. The `diff` mark accepts either a `hunks` list or the
flat single-hunk fields (`start`/`before`/`del`/`add`/`after`), the form the
`edit` builtin emits.

`AgentSink::emit` ([[map/exarch/shell-eval|shell-eval]]) is a two-decoder sink: an
`io`-keyed value goes through `value_to_io`/`io_card` to a `Kind::Io`
([[map/exarch/io-surface|io-surface]]), otherwise `value_to_card` runs and emits one
`Kind::Card` on the [[map/exarch/frontend|bus]]; detached workers buffer their
`surface` calls and replay them on `await`, so a card replays for free.

## Render — one interpreter, one binding table

`render_card(&Card, level)` (`tui/line.rs`) walks the marks; `role_style(Role)`
is the **single place hue lives** for kit content, so the kit can name a role but
never a colour, and magnitude can never land on hue — the encoding is correct by
construction. The quantitative encoders are reused, not duplicated: `measure`
calls the generalised `size_bar`/`progress_bar`, `diff` calls the patch body
(`diff_body`), and `fields` plus `provider_error` both feed the shared
`render_field_rows`/`push_field` matrix primitive — folding `provider_error` into
the `fields` path and killing the duplicate label-column logic. The header label
reads `diff`.

`render_card` opens with the single leading blank every block wears; the
data-encoding rail span is prepended by the [[map/exarch/frontend|block]] to the
first content row.

## Block — derived disclosure and aggregation

`BlockKind::Card(Card)` (`tui/block.rs`) replaces the old `Patch` variant and the
`task`/`meter`/`wrote` chrome. Disclosure is **derived**, not named: a card
holding a `diff` is dialable (`dialable()` → `Card::has_diff()`) and renders
L1 header / L3 full; a card of only `text`/`fields`/`measure`/`raw` is
chrome-level (L3-only, inert). The rail shape is `▎` for a diff card, `❖` for a
diff-less one. `magnitude()` is the summed diff magnitude, feeding the rail's
value-step and the agent matrix's size readout.

A single-`diff` card joins the patch-grouping buffer in `tui.rs`
(`Card::into_single_diff` → `absorb_patch`/`patch_buf`): consecutive same-`(id,
path)` diff cards merge their hunks into one `▎ diff <path>` block, the way a
unified diff presents one file. Every richer card is its own block, pushed via
`Viewport::push_card`.

## Machine log

`headless.rs` serialises a card to a structured mark tree in `transcript.jsonl`
(one `card` arm, the whole tree via serde); only a `raw` mark is opaque, and
honestly so. The stderr condenser (`card_stderr`) walks marks generically.

## Kit side

The tasks library holds small constructors so the mark grammar lives in one ral
place: `task-card`/`meter-card` in `kit/tasks.ral` (the kit owns the status→role
mapping, since the host knows only the closed role set), surfaced per transition.
The agent library's `edit` is now a Rust builtin
([[map/exarch/io-surface|io-surface]]) that builds its own `diff` card Rust-side;
the read/write redirect and exec cards are likewise composed from core's I/O
events, not by ral constructors — so the dormant `patch-card`/`wrote-card` ral
helpers are gone.

One ral constraint shapes the wire format: lists and records are statically
**homogeneous**. Heterogeneous *variant* lists are fine (ral unifies them into a
sum, so `` `card [text, measure, diff] `` typechecks), but within a homogeneous
list every record needs the same fields — so every `text` span carries a `role`
(use `""` for plain ink), and `fields` rows are records `[label:, value:]`, not
positional `[label, value]` pairs (which would force label and value to one
type). The `## Surfacing` section of `exarch/data/ral.md` teaches the model the
marks, the roles, and this constraint.

## See also

[[decisions/260619_surface-carries-documents|surface-carries-documents]] (the
decision), [[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]
(the chrome-side Bertin encoding this extends), [[map/exarch/frontend|frontend]]
(the bus / block / line arm this re-grounds), [[map/exarch/shell-eval|shell-eval]]
(the host sink that decodes the card), [[map/exarch|map: exarch]].
