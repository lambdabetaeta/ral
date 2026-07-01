## Surfacing

`surface CARD` shows the user a render document on the rail; use when a result is worth the user seeing it (a build summary, a test matrix, a captured output). Never repeat in words what you have surfaced.

A card `` `card LIST-OF-MARKS` `` is an ordered stack of marks drawn top-to-bottom. There are five marks:

- `` `text [spans: [[role: "…", text: "…"], …]] `` — a run of spans. Every span carries `role` — one of `path`, `code`, `ok`, `warn`, `bad`, `muted`, `strong` (identity, mapped to a hue), or `""` for plain ink. A heading is a `strong` span.
- `` `measure [label: "…", value: N, max: M, unit: "…"] `` — a magnitude. With `max`, it reads as a proportional bar (`value/max`); without, as a `log2` size bar. `max`/`unit` may be omitted.
- `` `fields [rows: [[label: "…", value: VALUE], …]] `` — an aligned `(label, value)` table; rows are records (a positional `[label, value]` list would force label and value to one type). A `VALUE` is a `` `text `` or `` `measure `` mark; use the same kind across the rows.
- `` `diff [path: "…", start: N, before: […], del: […], add: […], after: […]] `` — one located hunk (`del` rewritten to `add` at line `start`, with context). Pass `hunks: [[…], …]` for several. The host renders the size bar, add/del grain, and graded disclosure.
- `` `raw [bytes: "…"] `` — pre-formed bytes appended verbatim, for output outside the grammar. Honest about being un-encoded ink.

A `` `card `` may stack marks of different kinds, but within one homogeneous list — a span list, a `fields` row list — every element is one type, so give every span a `role` and keep a table'"'"'s values one kind. `edit` builds and surfaces its own diff card; the tasks kit adds `task-card`/`meter-card` constructors. **The backtick and tag name must be on the same line** (`` `card ``, not `` `\ncard ``); the payload may span lines freely. Compose marks directly for anything else:

    surface `card [
      `text    [spans: [[role: "strong", text: "tests "], [role: "ok", text: "42 passed"]]],
      `measure [label: "crates", value: 7, max: 12],
      `fields  [rows: [[label: "suite",  value: `text [spans: [[role: "",   text: "unit" ]]]],
                       [label: "status", value: `text [spans: [[role: "ok", text: "green"]]]]]]
    ]

    # A let binding keeps complex cards readable:
    let card = `card [
      `text    [spans: [[role: "strong", text: "tests "], [role: "ok", text: "42 passed"]]],
      `measure [label: "crates", value: 7, max: 12],
    ]
    surface $card
