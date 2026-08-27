---
generated_at_commit: cbeb5457
generated_at_date: 2026-08-17
covers_paths: [exarch/src/bus/card.rs, exarch/src/bus/card/diff.rs, exarch/src/bus/card/value.rs, exarch/src/bus/card/decode.rs, exarch/src/bus/card/encode.rs, exarch/src/bus/card/observation.rs, exarch/src/bus/card/done.rs, exarch/src/bus/card/notice.rs, exarch/src/bus/card/testkit.rs, exarch/src/shell_eval.rs, exarch/src/headless.rs, exarch/src/tui/line.rs, exarch/src/tui/palette.rs, exarch/src/tui/block.rs, exarch/src/tui/group.rs, exarch/src/tui/rail.rs, exarch/src/record.rs, exarch/src/record/commit.rs, exarch/src/record/view.rs, exarch/src/tui/viewport.rs, exarch/data/agent.ral]
---

# Map: exarch / cards

The `surface` builtin carries a **render document** — a `` `card ``, an ordered
stack of Bertin *marks* a kit composes entirely in ral. exarch decodes it once
into a closed Rust model and draws it through one generic interpreter. The *set
of cards* is open (compose marks, zero Rust per card — a surfaced file write, a
build summary, a test matrix, anything); the *set of marks* stays closed and
small, so the renderer is total and width-reflow, click-to-disclose, and patch
aggregation all keep working. This is
the [[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]]
discipline extended from the frontend's own chrome to the kit's *content*: the
kit declares **data and its level of measurement**, never appearance; exarch
owns the binding to visual variables. See
[[decisions/260619_surface-carries-documents|surface-carries-documents]].

## The marks

A `` `card `` is a `List` of marks rendered top-to-bottom on one scrollback
[[map/exarch/frontend|block]]. Five marks, closed (`exarch/src/bus/card.rs`):

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
- **`raw`** — un-encoded ink: pre-formed bytes appended verbatim, for output
  outside the grammar. Honest about being an image, not an encoding.

Composability is one rule at three scales: the plane stacks marks (`card`),
`fields` nests marks in its value column, `text` nests roles in its spans.

## Decode — `value_to_card`

`value_to_card` (`bus/card/decode.rs`) is the card decoder, reading marks off the runtime
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

## Encode — `encode_card`

`encode_card` (`bus/card/encode.rs`) is `value_to_card`'s inverse on the
decoder's image: `value_to_card(&encode_card(&card)) == card` for every `Card`
the decoder can produce. It exists because
[[decisions/260803_register-is-read-write|register-is-read-write]]'s
`pin-read` hands the model back a stored pin as a value it can destructure,
and that value must be **canonical**, not the bytes a kit happened to author:
a bare-string span or a bare mark — sugar the decoder accepts — comes back
through the tagged form (`` `text [spans: […]] ``), and an unknown mark comes
back as the plain text it already degraded to on the way in. Canonical
read-back is what makes storage and display one thing: a kit cannot smuggle
state through a payload the decoder discards, because the only card
`pin-read` can ever return is the one the rail already showed.

## Host-composed one-liners — `done`, notices

Some of what the marks describe is composed in Rust, never decoded from a
kit-authored `` `card ``. `settled_spans` words a detached worker's completion outcome — the `` `done ``
event core appends to every deferred surface batch, decoded by `value_to_done`
into a `DoneOutcome` and recorded as `Display::Done { outcome }` — as one
sentence, `background block settled (exit n)`, with a failure's message
appended. The outcome alone carries a level, roled `ok`/`bad` exactly as the
`$ cmd → status` exec row roles an exit code, which is what a settled block's
is: a non-settled `╳` rail marks a turn error (provider, stall, or forensic
fault) or a cancelled turn, while a nonzero worker exit remains the settled
outcome rather than a turn error.
`settled_text` flattens the spans for the two sinks with
no ink to spend — the headless tee and the model's wake-up notice
(`surface_notice`, [[map/exarch/agent|agent]]) — so none of the three can drift;
only `record::view`'s ledger keeps its own `[done: …]`, the bracketed register
every fact wears there. It names no worker, because there is nothing to name:
core spells a `spawn`'s `cmd` `<block>` and `prelude.ral`'s `defer` is a
`spawn`.

A settlement is *announced*, not bounded, so it is no card at all. Exarch's
transcript seats those spans as a chrome line on the rail — `ChromeKind::Settled`
lifts to the `↘` of `RailKind::Subagent`, since background work landing in
root's scrollback turns after the run that spawned it is the same event as an
agent's answer arriving, whatever produced it — and synod's fold drops
`Display::Done` unnarrated, a worker thread being exarch's own bookkeeping
rather than anything the window's reader has business with.

`notice_card` is `done`'s sibling for core's ready-boundary housekeeping (`value_to_notice` → a `Notice` recorded as `Display::Notice
{ notice }`): a `Notice::Reap` renders
through `reap_card` as a `warn` span plus the worker's `cmd` and which lease
fired ("idle 1h unobserved" / "24h backstop"), with prune and large-binding
notices rendered by its per-kind siblings
([[decisions/260706_enquiry-channel|enquiry-channel]]). All are fixed-position
value marks, never an animation, and all stay inside the existing `text` mark
vocabulary, so none widens the closed mark set above. `services_pin_card` is
the host-authored protected `services` pin ([[map/exarch/agent|agent]]).

A notice's raw fact is what reaches the record log — `Display::Notice
{ notice }` — exactly as a structural observation records only its raw wire
form (`Display::Observation`/`ObservationGroup`, [[map/exarch/io-surface|io-surface]]):
the card is a rendering, built fresh by whoever draws, and is never itself
recorded ([[map/exarch/agent|agent]]).

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
only `text`/`fields`/`measure`/`raw` is chrome-level (L3-only, inert). The rail
shape is `▎` for a file mutation — a diff card or a write card alike — and
none for a framed surfaced card; an observation card folds into its ral group
rather than carrying its own rail. `magnitude()` is the summed diff
magnitude, feeding the rail's value-step and the agent matrix's size readout;
`lines_changed()` exposes the same diff total as the matrix's write footprint,
distinct from prose volume.

A single-`diff` card joins the patch-grouping buffer in `record/commit.rs`
(`Card::into_single_diff` → `SurfaceBuffer::absorb_patch`): consecutive
same-`(id, path)` diff cards merge their hunks into one `diff <path>` block, the
way a unified diff presents one file. Every richer card is its own block, pushed
via `Viewport::push_card` (`tui/viewport.rs`); grouped observation effects land
via `push_observation_card`, a write card via `push_write_card`.

## Machine log

There is no independent operational trace any more: `record.jsonl`, written
through `record::Emitter` at the seam, is the one durable log, and its
`Display`/`Forensic` commits carry the structured facts a card renders from
— never the rendered card itself, which the view fold rebuilds on resume. The
headless stderr condenser (`card_stderr`, `headless.rs`) walks marks
generically off the live bus.

## Kit side

The kit declares data and its level of measurement; the host binds it to visual
variables, and with no host `surface` is the identity, so a kit stays runnable in
a bare ral REPL. The tasks library holds the small constructor so the mark
grammar lives in one ral place: `tasks-card` in `exarch/data/agent.ral` (the
tasks section; the kit owns the status→role mapping, since the host knows
only the closed role set), paired with `decode-tasks`, its inverse over the
same shape. It leads with a strong `text` heading — the same mark `set-goal`
writes — so the framed renderer lifts `tasks` into the top rule and the gauge
below it counts what is `completed`, rather than the label doing double duty as
a title. Every mutator reads the register through `decode-tasks
!{pin-read "tasks"}`, computes the new list, and writes it back through
`sync-tasks` — one write point wrapping `pin-set`/`pin-clear`
([[map/exarch/builtins|builtins]];
[[decisions/260803_register-is-read-write|register-is-read-write]]). The card
is the kit's only state; there is no bound list threaded alongside it to drift
from what's pinned.

The agent library's surfacing constructors are gone: `view-text`, `grep-files`,
and `edit-hash`/`edit-replace` are Rust host builtins
([[map/exarch/io-surface|io-surface]]), their file I/O sunk below the redirect
frame so each is one logical surface. An edit builds its own whole-file diff
card (one canonical original-vs-final diff grouped into hunks by `similar`) at
the edit, where both texts are already in hand; a committed `>` opens the head of what
landed against the empty side instead, an all-adds sample rather than a shape
of its own. Both are cut to ten rows and an `…` by `clip_hunks` where they are
composed, so a card is a report of a change and never a copy of the file — and
the renderer stays dumb: L1 is the header alone, every rung above it draws
what the mark holds. The read
redirect and exec cards are likewise composed from core's I/O events. `agent.ral` now carries
only the `-around` readers, the tasks kit, and the goal pins
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
core I/O events the sibling decoder turns into cards),
[[decisions/260803_register-is-read-write|register-is-read-write]] (the encoder
this page's cards feed, and the canonical-form rule it answers to),
[[design/pins|pins]] (the register `pin-read` answers from),
[[map/exarch|map: exarch]].
