---
generated_at_commit: 668499f
generated_at_date: 2026-07-12
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

**A runtime location carries a non-optional source identity, resolved once at
render.** A `SourceLoc` (`diagnostic.rs` — the script position of a runtime
error) carries the `FileId` of the source its `line`/`col` index — the script
or module active when the error was raised — alongside `len`. `SourceDb`
(`source.rs`) is the registry of every source the current turn has loaded,
keyed by `FileId`: it lives on `SessionState::sources` (a turn resets and
seeds it at turn start; hosts read it after the turn returns to render), while
the turn-local cursor — `LocationCursor` in `diagnostic.rs`, with the
active-source cache, the `current` id, and the `CallSite` snapshot — rides on
`TurnState::loc`. `Shell::install_script_context` registers each source and
makes it `current`; `ScriptContextGuard` (module loads, the REPL plugin
loader) saves and restores `current` around a load, and core's per-turn
`TurnGuard` swaps the whole cursor with the rest of the turn frame, while the
registered source stays in the db.

This is the structural fix for the cross-source caret: a runtime error raised
inside a `source`d module carries the module's `FileId`, so the renderer
resolves the **module's** text and draws the caret into the module's bytes —
not the top-level script's. An id the renderer cannot resolve (the placeholder
`FileId::DUMMY`, or an unregistered source) renders messageless rather than
indexing an unrelated text. ([[decisions/260614_structural-bug-prevention|structural-bug-prevention]] class 9.)

Parse and type errors render against the source they were just handed, so their
entry points still take `(file, source)` strings: a module's *compile* error is
surfaced by the loader as a plain message, never reaching the runtime renderer.

## Rendering — `core/src/diagnostic.rs`

All structured errors funnel through one module and render via the `ariadne`
crate with source-span underlining; when no span is available a compact
one-liner is used instead. The per-stage entry points are
`format_parse_error_ariadne`, `format_type_error_ariadne` (each taking
`(file, source)`), and `format_runtime_error_ariadne` / `format_runtime_error_auto`
(resolving a `SourceLoc` against a `SourceDb`) / `_compact`, with `cmd_error` and
`shell_warning` for unstructured command-layer output. Color is gated through
`ansi::use_color`.

**The raw ingredients of a span underline are exposed so an external renderer
can draw one in its own coordinate system.** `text::byte_to_char`
(`core/src/text.rs`, the shared UTF-8 boundary snappers — byte offset →
character offset, the unit ariadne and a `TextArea` cursor both count in) and
`TypeErrorKind::render_label` (`typecheck/explain.rs`, a kind → its under-caret
label phrase) are `pub`. The structural [[map/repl/frontend|frontend]] reuses
them to paint an in-place type-error underline whose label and caret agree
word-for-word and column-for-column with the post-Enter ariadne report — the
inline rendering belongs to that page, not here. Type-error *prose* generally
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
red stderr line in debug builds, nothing in release, no environment switch
([[decisions/260608_one-debug-path|one-debug-path]]). Its call sites are
permanent instrumentation.
