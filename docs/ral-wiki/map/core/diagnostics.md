---
generated_at_commit: 6d48e9af
generated_at_date: 2026-08-26
covers_paths: [core/src/source.rs, core/src/diagnostic.rs, core/src/text.rs, core/src/ansi.rs, core/src/exit_hints.rs]
---

# Map: core / diagnostics

How every user-visible message — parse errors, type errors, runtime errors,
exit-code hints — is located in source and rendered to the terminal.

## Source locations — `core/src/source.rs`

**A source position is a `Span`** — a half-open byte range `[start, end)`
tagged with a `FileId`, an opaque per-file handle. Line and column are *not*
carried on [[map/core/ir|IR]] or AST nodes; they are recovered at render time
by handing the source text directly to `ariadne`. Where no narrower position is
available, the position is `Option<Span> = None` uniformly across the
AST, IR, and typechecker. `Span::join` merges two spans; `Span::range` yields
the `std::ops::Range<usize>` ariadne expects. Carrying only byte offsets keeps
the IR free of presentation state.

## Runtime source identity — `SourceDb`

**A runtime error carries the span of the node it broke on, not a cursor the
evaluator wrote down.** `Error` (`types/error.rs`) holds
`span: Option<Span>` — `None` at mint, stamped by the break path with the
innermost enclosing node's span as it unwinds (`Error::at_span`). Since a
`Span` is already tagged with its `FileId`, the source identity is the
value's own and no ambient position has to be maintained.

`SourceDb` (`source.rs`) resolves that id at render time. It is the registry
of every source the *session* has loaded, keyed by `FileId`, living on
`SessionState::sources`: **append-only for the session's whole life**, so a
nested run can never re-mint a `FileId` an outer run's live spans still
name. `Shell::install_script_context` registers a source and
`install_root_context` additionally seeds `SessionState::root_file` with the
current run's root (`FileId::DUMMY` between runs); `SourceDb::next_id` peeks
the id a registration will mint, so a compiler can stamp a program's spans
before the source they name is itself in the db. Hosts read the db after a
run returns to render.

This is the structural fix for the cross-source caret: a runtime error raised
inside a `source`d module carries the module's `FileId`, so the renderer
resolves the **module's** text and draws the caret into the module's bytes —
not the top-level script's. An id the renderer cannot resolve (the placeholder
`FileId::DUMMY`, or an unregistered source) renders messageless rather than
indexing an unrelated text. ([[decisions/260614_structural-bug-prevention|structural-bug-prevention]] class 9.)

`CallSite` (`diagnostic.rs`) is the audit-and-wire shape a `Span` resolves
*to* — script name plus 1-indexed `(line, col)` — which hosts read off every
observation, command or capability check alike. It rides the
[[map/core/shell-state|audit collector]] rather than the run frame, so an
observation carries the position of the dispatch that produced it.

Parse and type errors render against the source they were just handed, so their
entry points still take `(file, source)` strings: a module's *compile* error is
surfaced by the loader as a plain message, never reaching the runtime renderer.

## Rendering — `core/src/diagnostic.rs`

All structured errors funnel through one module and render via the `ariadne`
crate with source-span underlining; when no span is available a compact
one-liner is used instead. The per-stage entry points are
`format_parse_error_ariadne`, `format_type_error_ariadne` (each taking
`(file, source)`), and `format_runtime_error_ariadne` / `format_runtime_error_auto`
(resolving the error's `Span` against a `SourceDb`) / `_compact`, with `cmd_error` and
`shell_warning` for unstructured command-layer output. Color is gated through
`ansi::use_color`.

`format_runtime_error_auto` picks between the two by asking where the error
came from, not what the input looked like: it takes `compact_root:
Option<FileId>` — `Some(root)` when the input compiled to a single command,
carrying that input's own id — and renders compact only when the error's span
is absent or names `root`. A single command that dispatches into an rc alias,
a `source`d function or a lambda from an earlier run faults in text the user
cannot see, so it gets the caret.

**The raw ingredients of a span underline are exposed so an external renderer
can draw one in its own coordinate system.** `text::byte_to_char`
(`core/src/text.rs`, the shared UTF-8 boundary snappers — byte offset →
character offset, the unit ariadne and a `TextArea` cursor both count in) and
`TypeErrorKind::render_label` (`typecheck/explain.rs`, a kind → its under-caret
label phrase) are `pub`. The structural [[map/repl/frontend|frontend]] reuses
them to paint an in-place type-error underline whose label and caret agree
word-for-word and column-for-column with the post-Enter ariadne report — the
inline rendering belongs to that page, not here. `text.rs` is also the single
home of the `nucleo` fuzzy matcher (`rank`, and `rank_by` for an item that is
not its own haystack), so every filtered list a user is offered — completion
menus, pickers, the exarch command popup — ranks the same way. Type-error *prose* generally
lives beside the checker now: provenance is data on the error (`Reason`,
`typecheck/error.rs`) and every user-facing sentence is a pure function of it
in `typecheck/explain.rs` ([[map/core/typecheck|typecheck]]).

## Styling — `core/src/ansi.rs`

Escape constants and the color-gating predicates `use_color` / `use_ui_color`,
which consult a `TerminalState` cached once at REPL startup via `set_terminal`.
When the cache is empty `use_color` falls back to inline probing (batch runs and
early-startup errors); `use_ui_color` is cache-only and yields false until
`set_terminal` has run. Also the OSC
helpers: `osc_set_title`, `osc8_link`, `osc52_copy`. Value-output styling (the
REPL's `=> ` prefix) lives instead in the `ral` crate's
[[map/repl/loop|repl::theme]].

## Exit-code hints — `core/src/exit_hints.rs`

`ExitHints` is a pure `(command, exit-status) → explanation` lookup table,
populated via `from_text` and installed into the `Shell`; `lookup` is consulted
when an external command fails. Loading the table is the caller's concern.

## Debug tracing — `dbg_trace!`

`dbg_trace!(tag, …)` is the single developer-facing trace primitive: a tagged
stderr line in debug builds (red only where the `ansi` colour gate allows),
nothing in release, no environment switch for the trace itself
([[decisions/260608_one-debug-path|one-debug-path]]). Its call sites are
permanent instrumentation.
