---
generated_at_commit: ad2123a08dfd12616ada6318e9740042ab7e9e37
generated_at_date: 2026-11-06
covers_paths: [core/src/source.rs, core/src/diagnostic.rs, core/src/compile.rs, core/src/syntax/report.rs, core/src/io/terminal.rs, core/src/io/terminal/stderr.rs, core/src/text.rs, core/src/io/ansi.rs, core/src/shell/runtime/exit_hints.rs, core/src/shell/runtime/error.rs, core/src/shell/runtime/flow.rs]
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

**A runtime error carries one provenance: the failing frame's name and site.**
`Error` (`shell/runtime/error.rs`) holds `prov: Option<Prov>` until located.
`Break::locate(name, span, shell)` claims an unlocated error or stop only when
its span lies in a session source. `Prov::Live { name, span }` draws a caret;
`Prov::Resolved` holds a name and `CallSite`, drawing coordinates without a
caret. An initial `_` does not hide a frame; the baked prelude is invisible
because its spans are outside session sources. See
[[decisions/261105_the-error-record|the error record]].

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
`register_root` also returns the run's root span, which `IoLoan` installs
with `SessionState::root_file` (`None` between runs); `SourceDb::next_id` peeks
the id a registration will mint, so a compiler can stamp a program's spans
before the source they name is itself in the db. Hosts read the db after a
run returns to render.

This is the structural fix for the cross-source caret: a runtime error raised
inside a `use`d module carries the module's `FileId`, so the renderer
resolves the **module's** text and draws the caret into the module's bytes —
not the top-level script's. An id the renderer cannot resolve (the placeholder
`FileId::DUMMY`, or an unregistered source) renders messageless rather than
indexing an unrelated text. ([[decisions/260614_structural-bug-prevention|structural-bug-prevention]] class 9.)

`CallSite` (`source.rs`, resolved by `SourceDb::site`) is the audit-and-wire
shape a span resolves to: script name and 1-indexed `(line, col)`.
`Audit` holds one total, private `Prov` register for the innermost visible
frame; `enter_site` displaces it for a `DisplacedSite` token that only
`restore_site` consumes, and visible calls bracket it so, with adjacent
tail-call restorations coalesced. `Shell::at_site(name, span, body)` brackets synchronous operations.
Every source run installs its root span; hooks install their origin at 1:1;
workers inherit the parent's register at spawn. `Observation.site` and
`Shell::call_site()` are total `CallSite`s. Minting an error record reads its
own provenance, else that register, so an unclaimed failure names the run's
start. A worker completes provenance before publishing, making `done`, `poll`
and `await` agree.

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
Its `Reason::Raised` carries the plain compile sentence a `try` handler reads
with `describe`; a loader's context prefix belongs only to the report.
`locate` supplies the `use` frame's provenance, so `$err[site]` names the
`use` that failed, while the rejection draws the loaded source's compile
failure.

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
(`shell/runtime/error.rs`, resolving live provenance against a `SourceDb`
and borrowing the session's `ExitHints`).
Rendering tests live inline with each owner: generic renderer tests in
`diagnostic.rs`, parse and lexer reports in `syntax/report.rs`, type reports in
`typecheck/explain.rs`, and runtime source selection in `shell/runtime/error.rs`.

**`End::render` draws an ending through one private `Diagnosis` and one
`render` function** in `shell/runtime/error.rs`. `Error::diagnosis` supplies
the described reason and report-only facts; `Stop::diagnosis` supplies its
cause and provenance. `Reason::describe(&self)` is the one wording, reading
the reason alone; `Error::describe` and `ErrorRecord::describe` delegate.
Reason hints (`Reason::hint(&self)`: signal, path-as-text on `program`, session
exit-hints lookup by `program` basename and status) are recomputed; a stored
situational hint takes precedence. Context prefixes never change the reason. The renderer
composes hint, saturating counted discard, then eligible abandoned-tail note.
A panic is spanless `run panicked: …`; values, exits and reader-gone stops
render nothing. `Error::render`, `compact` and `render_spanless` use that same
renderer. Hosts print the supplied `rendered` verbatim.

The renderer dispatches on where the error came from, not what the input
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
`TerminalState` stores base facts only: `hyperlinks`, `clipboard_write` (both `modern_osc`) and
`bracketed_paste` (`supports_ansi`) are methods, so no two fields can disagree, and the
`$TERMINAL` keys of those names are derived.
Value-output styling (the REPL's `=> ` prefix) lives instead in the `ral`
crate's [[map/repl/loop|repl::theme]].

## Exit-code hints — `core/src/shell/runtime/exit_hints.rs`

`ExitHints` is a pure `(program basename, exit-status) → explanation` lookup table
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
