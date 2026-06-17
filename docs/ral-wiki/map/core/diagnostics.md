---
generated_at_commit: 1f8cb95d
generated_at_date: 2026-06-15
covers_paths: [core/src/source.rs, core/src/diagnostic.rs, core/src/ansi.rs, core/src/exit_hints.rs]
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
render.** A `SourceLoc` (script position of a runtime error) carries the
`FileId` of the source its `line`/`col` index — the script or module active
when the error was raised — alongside `len`. `SourceDb` is the registry of
every source the session has loaded, keyed by `FileId`; it lives on
`Location` (with the active-source cache and the `current` id) and accumulates
as scripts and modules install their context. `Location::install_script_context`
registers each source and makes it `current`; `ScriptContextGuard` (module
loads, the REPL plugin loader) and exarch's `IoGuard` save and restore
`current` around the load, while the registered source stays in the db.

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
`(file, source)`), and `format_runtime_error_ariadne` / `_auto` (resolving a
`SourceLoc` against a `SourceDb`) / `_compact`, with `cmd_error` and
`shell_warning` for unstructured command-layer output. Color is gated through
`ansi::use_color`.

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
