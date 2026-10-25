---
generated_at_commit: a2aba680
generated_at_date: 2026-10-25
covers_paths: [core/src/source.rs, core/src/diagnostic.rs, core/src/compile.rs, core/src/syntax/report.rs, core/src/io/terminal.rs, core/src/io/terminal/stderr.rs, core/src/text.rs, core/src/io/ansi.rs, core/src/shell/runtime/exit_hints.rs]
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
evaluator wrote down.** `Error` (`shell/runtime/error.rs`) holds
`span: Option<Span>` — `None` at mint, stamped by the break path with the
innermost enclosing node's span as it unwinds (`Error::at_span`). Since a
`Span` is already tagged with its `FileId`, the source identity is the
value's own and no ambient position has to be maintained.

Every registered `Source` carries its `SourceOrigin` (`source.rs`): a
`File` by the native path it was read from, or a `Virtual` label —
`<stdin>`, `-c`, a hook, an embedded profile — for text no file holds.
That path is a `SourceFile`, UTF-8 by construction: an origin travels
serialized (on a `Run`, the module stack, the `SourceDb`) and serde holds a
path only as text, so `SourceFile::new` — the one door, through
`path::utf8` — refuses a path that is not UTF-8, naming it, before the file
is read, rather than letting serialization fail late. Every loader (batch
script, startup file, plugin, `use`, capability profile, exarch's config and
network policy) surfaces that refusal as its own error. The
origin is the source's identity: the module stack's cycle key, the
directory a script-relative path resolves beside, and, for a file, what
`$SCRIPT` bakes. The display name a diagnostic draws and a `CallSite`
carries is derived from it once, at registration: a file's path
`path::shown`, a virtual source's label as given. A host names a run's
root source by the `origin` on its `Run`.

`SourceDb` (`source.rs`) resolves that id at render time. It is the registry
of every source the *session* has loaded, keyed by `FileId`, living on
`SessionState::sources`: **append-only for the session's whole life**, so a
nested run can never re-mint a `FileId` an outer run's live spans still
name. `Shell::install_script_context` registers a source and
`install_root_context` additionally seeds `SessionState::root_file` with the
current run's root (`None` between runs); `SourceDb::next_id` peeks
the id a registration will mint, so a compiler can stamp a program's spans
before the source they name is itself in the db. Hosts read the db after a
run returns to render.

This is the structural fix for the cross-source caret: a runtime error raised
inside a `use`d module carries the module's `FileId`, so the renderer
resolves the **module's** text and draws the caret into the module's bytes —
not the top-level script's. An id the renderer cannot resolve (the placeholder
`FileId::DUMMY`, or an unregistered source) renders messageless rather than
indexing an unrelated text. ([[decisions/260614_structural-bug-prevention|structural-bug-prevention]] class 9.)

`CallSite` (`source.rs`, resolved by `SourceDb::site`) is the audit-and-wire shape a `Span` resolves
*to* — script name plus 1-indexed `(line, col)` — which hosts read off every
observation, command or capability check alike. It rides the
[[map/core/shell-state|audit collector]] rather than the run frame, so an
observation carries the position of the dispatch that produced it. Both a
command and a function application stamp it, but one whose span resolves
outside the session's sources — the baked prelude's — leaves it alone, so
a prelude function's inner command is stamped where the user called the function. A dispatch with no position is
`None` (`Shell::site_of`, `Observation.site`), and the ral records that
project it — a trail entry, `try`'s error — carry an optional
`` site: `some [script, line, col] | `none ``.

**A compile failure carries its own text**: `Rejection` (`diagnostic.rs`) pairs
the failure's `Report`s with their `Source` and the status a run that failed so
exits on (2 parse, 1 type). A root run registers only after compilation
succeeds; runtime loads register before compiling so every stamped span resolves
in the session. `compile::CompileError` (`Parse` or `Types`) becomes a rejection
with `reject`. A run that never reached evaluation carries one in
its `StaticDiagnostics` (`shell/run.rs`) — `Compile(Rejection)`, beside `Host`, a
spanless pre-run failure (an unknown hook, a non-ground argument) — whose
`render` is what every host on the wire prints through `Report::Static`
([[map/core/engine-protocol|engine-protocol]]). `Compile` is not folded into
`Error`: a loaded file's parse error would then raise status 2, not 1. A file
loaded at runtime — a `use`d module, the rc, a plugin, a capability profile —
carries its rejection on the runtime `Error` instead, as `Error::rejection`,
from `Error::compile` (`shell/runtime/error.rs`) at the loader's compile door
(`shell::load::check_source`); `CompileError` itself knows no runtime error.
Its `message` stays the plain sentence a `try` handler reads
(`use: parse error: …`), and its `span` is left to the break path's stamp,
so `$err[site]` names the `use` that failed, while the rejection draws the
loaded source's compile failure.

## Rendering — `core/src/diagnostic.rs`

**Each error draws itself; one drawer draws all.** `diagnostic` is the report
algebra: a `Report` (code, message, primary `Label`, optional secondary, hint)
and `Rejection`. `Report::render(&Source)` is the single ariadne call, with
byte indexing (`IndexType::Byte`) so a `Span` is the label's own coordinate;
`caret` widens an empty span to one whole char and anchors end of input on the
last char, so every label is drawable. With no primary label, `Report::plain`
is the compact one-liner. The builders sit with their errors:
`ParseError::report` in `syntax/report.rs` (the `L000x` messages come from
`LexErrorKind::headline` alone; a plain `P0001` carries its own help, the
message saying what is wrong and the help what to write), `TypeError::report` in
`typecheck/explain.rs`, and `Error::render` / `Error::compact`
(`shell/runtime/error.rs`, resolving the error's `Span` against a `SourceDb`).
Rendering tests live inline with each owner: generic renderer tests in
`diagnostic.rs`, parse and lexer reports in `syntax/report.rs`, type reports in
`typecheck/explain.rs`, and runtime source selection in `shell/runtime/error.rs`.

`Error::render` dispatches on where the error came from, not what the input
looked like: a carried `rejection` draws its own report, so a broken plugin,
module, rc or profile prints, at every door, the report its own run as a script
would. Otherwise it takes `compact_root: Option<FileId>` — `Some(root)` when
the input compiled to a single command, carrying that input's own id — and
renders compact only when the error's span is absent or names `root`. A single
command that dispatches into an rc alias, a `use`d function or a lambda from an
earlier run faults in text the user cannot see, so it gets the caret.

The shell's own stderr lines sit below the renderer, in
`terminal/stderr.rs`: `cmd_error`, `shell_warning`, and the macros `outln!` /
`err!` / `errln!`, exported for the front ends' own output too. Unlike
`println!` and `eprintln!` they drop a failed write rather than panic, since
once the terminal hangs up every write fails with EIO; `clippy.toml` bans the
std pair.

**The raw ingredients of a span underline are exposed so an external renderer
can draw one in its own coordinate system.** `text::byte_to_char`
(`core/src/text.rs`, byte offset →
character offset, the unit ariadne and a `TextArea` cursor both count in) and
`TypeErrorKind::render_label` (`typecheck/explain.rs`, a kind → its under-caret
label phrase) are `pub`. The structural [[map/repl/frontend|frontend]] reuses
them to paint an in-place type-error underline whose label and caret agree
word-for-word and column-for-column with the post-Enter ariadne report — the
inline rendering belongs to that page, not here. `text.rs` is also the single
home of the `nucleo` fuzzy matcher (`rank`, and `rank_by` for an item that is
not its own haystack), which serves exarch's pickers and command popup. Type-error *prose* generally
lives beside the checker: provenance is data on the error (`Reason`,
`typecheck/error.rs`) and every user-facing sentence is a pure function of it
in `typecheck/explain.rs` ([[map/core/typecheck|typecheck]]).

## Styling and the colour gate — `core/src/io/ansi.rs`, `core/src/io/terminal.rs`

`ansi` is vocabulary only: escape constants, the OSC helpers (`osc_set_title`,
`osc8_link`), `escape_seq_len`, `strip`, `visible`, `when`.
**One colour decision, made once**: a front end probes a `TerminalState` and
`seat`s it (`TerminalState::seat`, first wins); `terminal::stderr_color` and
`terminal::ui_color` read that seat. A wire engine seats its `EngineSpec.terminal`,
the front end's, never a probe of its own null stdio. Before any seat,
`stderr_color` answers from one memoised `Auto` probe, so early-startup errors
still colour, and `ui_color` is false: only a front end has a UI. The
`$TERMINAL` map is `impl From<&TerminalState> for Value` (`shell/runtime/value.rs`).
Value-output styling (the REPL's `=> ` prefix) lives instead in the `ral`
crate's [[map/repl/loop|repl::theme]].

## Exit-code hints — `core/src/shell/runtime/exit_hints.rs`

`ExitHints` is a pure `(command basename, exit-status) → explanation` lookup table
(two maps, `*` the wildcard),
populated via `from_text` and installed into the `Shell`; `lookup` is consulted
when an external command fails. Loading the table is the caller's concern.

## Debug tracing — `dbg_trace!`

`dbg_trace!(tag, …)` is the single developer-facing trace primitive: a tagged
stderr line in debug builds (red only where the `terminal::stderr_color` gate allows,
and through `errln!`, so never a panic),
nothing in release, no environment switch for the trace itself
([[decisions/260608_one-debug-path|one-debug-path]]). Its call sites are
permanent instrumentation.
