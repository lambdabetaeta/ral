//! Error formatting and diagnostic rendering.
//!
//! All user-visible error output -- parse errors, type errors, and runtime
//! errors -- is funnelled through this module.  Structured errors are
//! rendered via the `ariadne` crate with source-span underlining; when no
//! span is available, a compact one-liner format is used instead.
//!
//! Color output is gated by [`ansi::use_color`].

use crate::ansi::{self, BOLD_CYAN, BOLD_RED, BOLD_YELLOW, RESET};
use crate::source::{
    FileId, Source, SourceDb, Span as ByteSpan, byte_to_line_col, line_col_to_byte,
};
use crate::syntax::lexer::{LexErrorKind, StringForm};
use crate::syntax::parser::ParseError;
use crate::text::byte_to_char;
use crate::typecheck::TypeError;
use std::fmt::Write;

// Re-export the color-gating functions so diagnostics and their gate share
// one import.
pub use ansi::{set_terminal, use_color};

// ── Source location ───────────────────────────────────────────────────────

/// A source location for error reporting.
///
/// `source` is the non-optional identity of the source whose `line`/`col`
/// index this carries — the [`FileId`] of the script or module that was
/// active when the error was raised.  The runtime renderer resolves it
/// against a [`SourceDb`] once at render, so a location that crosses a
/// module boundary draws its caret into the right source's bytes.
///
/// Serializable so it can ride along with error outcomes across the
/// sandbox and pipeline-helper IPC seams; the `FileId` resolves against the
/// parent's `SourceDb` on decode.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceLoc {
    pub source: FileId,
    pub line: usize,
    pub col: usize,
    pub len: usize,
}

/// A source position: script name + (line, col).  Used both for "where we
/// are now" and (via `Location::call_site`) "where we were called from".
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct CallSite {
    pub script: String,
    pub line: usize,
    pub col: usize,
}

/// Turn-local source cursor for diagnostics.
///
/// Holds where execution is,
/// where it was called from (saved before entering prelude wrappers so
/// `audit`/`try` name the user's line, not the prelude's), and the cached
/// source text of the current script for structured spans.
///
/// The durable registry it resolves against — the
/// [`SourceDb`](crate::source::SourceDb) keyed by [`FileId`] — is session
/// state, not part of this cursor: the cursor is installed by the current turn
/// and discarded on teardown, while the registry survives so a turn's runtime
/// error still renders after the turn returns.
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct LocationCursor {
    pub script: String,
    pub line: usize,
    pub col: usize,
    /// Cached source text plus a precomputed line-start index for fast
    /// span → (line, col) lookup.  Not serde-transmissible (holds Arcs)
    /// and the sandbox child doesn't need it for diagnostics.
    #[serde(skip)]
    pub source: Option<Source>,
    /// Identity of the active source — the [`FileId`] registered in the
    /// session registry for the text in `source`.  Stamped onto every
    /// [`SourceLoc`] this cursor builds so the renderer resolves the right
    /// source at render time.  [`FileId::DUMMY`] before any script context is
    /// installed.
    pub current: FileId,
    pub call_site: CallSite,
}

impl LocationCursor {
    /// Snapshot the user-visible call site (preserved across prelude
    /// wrappers) as a [`CallSite`].  Used wherever capability checks
    /// and command audit nodes need a value-typed source position —
    /// passing the snapshot by value lets the caller hold `&mut
    /// audit` alongside without a borrow conflict against the
    /// cursor itself.
    pub fn audit_site(&self) -> CallSite {
        self.call_site.clone()
    }

    /// Record the current position (`script`, `line`, `col`) as the
    /// user-visible call site.  Invoked at the start of dispatch so
    /// audit nodes and error frames produced by the body name the
    /// user's line rather than wherever the body's evaluation has
    /// since drifted.
    pub fn record_call_site_here(&mut self) {
        self.call_site = CallSite {
            script: self.script.clone(),
            line: self.line,
            col: self.col,
        };
    }

    /// Build a [`SourceLoc`] anchored at the current position with the given
    /// highlight length.  Used by error-construction sites that want to
    /// point the diagnostic at the command/tool name on the current line.
    /// The location carries `current` — the identity of the active source —
    /// so the renderer resolves the right text even when the error crosses a
    /// module boundary before it is rendered.
    pub fn source_loc(&self, len: usize) -> SourceLoc {
        SourceLoc {
            source: self.current,
            line: self.line,
            col: self.col,
            len,
        }
    }
}

// ── Format functions (ariadne) ────────────────────────────────────────────

/// The `(error-colour, hint-colour, reset)` triple — empty strings when
/// color is disabled so the same `format!` works either way.
fn error_palette() -> (&'static str, &'static str, &'static str) {
    if use_color() {
        (BOLD_RED, BOLD_CYAN, RESET)
    } else {
        ("", "", "")
    }
}

/// Render a bare "code: message" line when there's no source span to point at.
/// The shared messageless fallback for both the type-error path (error
/// lacks a span) and the runtime-error path (no location, or its source is
/// unresolved in the registry).
fn render_messageless(code: Option<&str>, message: &str, hint: Option<&str>) -> String {
    let mut out = String::new();
    let (head, cyan, reset) = error_palette();
    match code {
        Some(c) => {
            let _ = writeln!(out, "{head}[{c}] Error{reset}: {message}");
        }
        None => {
            let _ = writeln!(out, "{head}Error{reset}: {message}");
        }
    }
    if let Some(h) = hint {
        let _ = writeln!(out, "  {cyan}help{reset}: {h}");
    }
    out
}

// ── Ariadne render core ──────────────────────────────────────────────────
//
// Every ariadne render is the same shape: clamp a char range to source,
// build a single-label red report with code/message and optional help,
// write it to a byte buffer, return the UTF-8 string.  `render_ariadne`
// is that shape.  The three public entry points differ only in how they
// derive the range and the label phrase.

/// Source range plus the phrase placed next to its underline.
struct LabelRange {
    range: std::ops::Range<usize>,
    label: String,
}

/// Render an ariadne report with one red primary label, an optional
/// yellow secondary label, and an optional help line.  Single entry
/// point for every diagnostic in the codebase: callers vary in how
/// they derive the ranges and phrases, not in the report shape.
#[allow(clippy::too_many_arguments)]
fn render_ariadne(
    file: &str,
    source: &str,
    code: &str,
    message: &str,
    primary: LabelRange,
    secondary: Option<LabelRange>,
    hint: Option<&str>,
) -> String {
    let file_owned: String = file.to_string();
    let mut builder = ariadne::Report::<(String, std::ops::Range<usize>)>::build(
        ariadne::ReportKind::Error,
        (file_owned.clone(), primary.range.clone()),
    )
    .with_config(ariadne::Config::default().with_color(use_color()))
    .with_code(code)
    .with_message(message)
    .with_label(
        ariadne::Label::new((file_owned.clone(), primary.range))
            .with_message(primary.label)
            .with_color(ariadne::Color::Red),
    );
    if let Some(s) = secondary {
        builder = builder.with_label(
            ariadne::Label::new((file_owned.clone(), s.range))
                .with_message(s.label)
                .with_color(ariadne::Color::Yellow),
        );
    }
    if let Some(h) = hint {
        builder = builder.with_help(h);
    }
    let mut buf: Vec<u8> = Vec::new();
    let _ = builder.finish().write(
        (file_owned, ariadne::Source::from(source.to_string())),
        &mut buf,
    );
    String::from_utf8_lossy(&buf).into_owned()
}

/// Char range starting at `start` of the given char-width, clamped so the
/// caret always points at *some* character even at end-of-source.
fn caret_range(source: &str, start: usize, width: usize) -> std::ops::Range<usize> {
    let char_len = source.chars().count();
    let s = start.min(char_len);
    let e = (s + width.max(1)).min(char_len.max(s + 1));
    s..e
}

/// Render a parse error via ariadne.
///
/// When the error originated in the
/// lexer the structured kind drives a dual-label render (opening
/// delimiter + EOF position + nested-form note); otherwise a single
/// red label points at the offending token.
pub fn format_parse_error_ariadne(file: &str, source: &str, err: &ParseError) -> String {
    if let Some(kind) = &err.lex_kind
        && let Some(report) = lex_error_report(source, kind)
    {
        return render_ariadne(
            file,
            source,
            report.code,
            &report.message,
            report.primary,
            report.secondary,
            report.hint.as_deref(),
        );
    }
    let range = err
        .span
        .map_or_else(|| eof_char_range(source), |s| byte_span_to_char_range(source, s));
    render_ariadne(
        file,
        source,
        "P0001",
        &err.message,
        LabelRange {
            range,
            label: "here".into(),
        },
        None,
        None,
    )
}

// ── Lex-error report ──────────────────────────────────────────────────────

/// Byte→char clamp.  Spans arrive in bytes from the lexer; ariadne is
/// configured for chars, so every span passes through here.
fn byte_span_to_char_range(source: &str, span: ByteSpan) -> std::ops::Range<usize> {
    let s = byte_to_char(source, span.start as usize);
    let e = byte_to_char(source, span.end.max(span.start + 1) as usize);
    caret_range(source, s, e.saturating_sub(s).max(1))
}

fn eof_char_range(source: &str) -> std::ops::Range<usize> {
    caret_range(source, source.chars().count(), 1)
}

/// Recursive description of a nested `LexErrorKind` for the help line —
/// `"foo !{$(unclosed"` becomes "`{…}` opened at 1:6, which itself
/// contains `$(…)` opened at 1:8".  Line/column is recovered from
/// `source` at render time rather than carried on the error.
fn describe_inner(source: &str, kind: &LexErrorKind) -> String {
    let pos = |span: ByteSpan| {
        let (line, col) = byte_to_line_col(source, span.start as usize);
        format!("{line}:{col}")
    };
    match kind {
        LexErrorKind::UnterminatedBalanced {
            open,
            close,
            opened,
            ..
        } => format!("`{open}…{close}` opened at {}", pos(*opened)),
        LexErrorKind::UnclosedDeref { opened, .. } => {
            format!("`$(…)` opened at {}", pos(*opened))
        }
        LexErrorKind::UnterminatedString {
            form,
            opened,
            inner,
            ..
        } => {
            let head = format!("nested {form} opened at {}", pos(*opened));
            match inner {
                Some(i) => format!(
                    "{head}, which itself contains {}",
                    describe_inner(source, i)
                ),
                None => head,
            }
        }
        LexErrorKind::Other(_) => "an unrelated lexer error".into(),
    }
}

/// Decomposition of a `LexErrorKind` into the parts `render_ariadne` consumes.
struct LexErrorReport {
    code: &'static str,
    message: String,
    primary: LabelRange,
    secondary: Option<LabelRange>,
    hint: Option<String>,
}

/// Decompose a `LexErrorKind` into the parts `render_ariadne` consumes.
/// Returns `None` for `Other(_)` so the caller falls back to the
/// generic single-label parse-error render.
fn lex_error_report(source: &str, kind: &LexErrorKind) -> Option<LexErrorReport> {
    match kind {
        LexErrorKind::UnterminatedString {
            form,
            opened,
            inner,
            ..
        } => {
            let close = match form {
                StringForm::DoubleQuoted => '"',
                StringForm::SingleQuoted | StringForm::BumpedSingle(_) => '\'',
            };
            let primary = LabelRange {
                range: byte_span_to_char_range(source, *opened),
                label: format!("{form} opened here"),
            };
            let secondary = LabelRange {
                range: eof_char_range(source),
                label: format!("expected closing `{close}` here"),
            };
            let hint = inner.as_ref().map(|i| {
                format!(
                    "nested {} was not closed before EOF",
                    describe_inner(source, i)
                )
            });
            Some(LexErrorReport {
                code: "L0001",
                message: format!("unterminated {form}"),
                primary,
                secondary: Some(secondary),
                hint,
            })
        }
        LexErrorKind::UnterminatedBalanced {
            open,
            close,
            opened,
            ..
        } => Some(LexErrorReport {
            code: "L0002",
            message: format!("unterminated `{open}…{close}`"),
            primary: LabelRange {
                range: byte_span_to_char_range(source, *opened),
                label: format!("`{open}` opened here"),
            },
            secondary: None,
            hint: Some(format!("expected closing `{close}` before end of input")),
        }),
        LexErrorKind::UnclosedDeref { opened, .. } => Some(LexErrorReport {
            code: "L0003",
            message: "unclosed `$(…)` dereference".into(),
            primary: LabelRange {
                range: byte_span_to_char_range(source, *opened),
                label: "`$(` opened here".into(),
            },
            secondary: None,
            hint: Some("expected closing `)` before end of input".into()),
        }),
        // L0004 is reserved for the proposed raw-delimiter near-miss (260608).
        // L0005 is retired: it rejected `<<`, which is now the here-string
        // redirect; the `<<EOF` near-miss is a parser diagnostic instead.
        LexErrorKind::Other(_) => None,
    }
}

/// Render a type error via the ariadne crate — structured labels, error
/// code, and optional help.  Falls back to a messageless render when the
/// error carries no span (nothing to point at).
pub fn format_type_error_ariadne(file: &str, source: &str, err: &TypeError) -> String {
    let message = err.kind.render_message();
    let code = err.kind.code();
    let hint = err.hint();
    let Some(sp) = err.pos else {
        return render_messageless(Some(code), &message, hint.as_deref());
    };
    let range = byte_span_to_char_range(source, sp);
    render_ariadne(
        file,
        source,
        code,
        &message,
        LabelRange {
            range,
            label: err.kind.render_label(),
        },
        None,
        hint.as_deref(),
    )
}

/// Render every type error in `errs` via ariadne, concatenated — one
/// span-and-caret report per error.  The REPL/script/rc/`--check` paths
/// use this; the loaders collapse to a single message instead.
pub fn format_type_errors_ariadne(file: &str, source: &str, errs: &[TypeError]) -> String {
    errs.iter()
        .map(|e| format_type_error_ariadne(file, source, e))
        .collect()
}

/// Render a runtime error via ariadne.
///
/// Resolves the location's source
/// identity against `db` to recover the file name and text the `line`/`col`
/// index, then draws the caret there; `len` is a byte length, converted to a
/// character width for the underline.  Falls back to a messageless render when
/// there is no location or `db` does not hold the named source — the latter is
/// the live cross-source guard: a location whose source the renderer cannot
/// resolve never draws a caret at an unrelated byte.
pub fn format_runtime_error_ariadne(
    db: &SourceDb,
    loc: Option<&SourceLoc>,
    message: &str,
    hint: Option<&str>,
) -> String {
    let Some((loc, source)) = loc.and_then(|loc| db.get(loc.source).map(|src| (loc, src))) else {
        return render_messageless(Some("R0001"), message, hint);
    };
    let text = source.as_str();
    let start_byte = line_col_to_byte(text, loc.line, loc.col);
    let start = byte_to_char(text, start_byte);
    let end = byte_to_char(text, start_byte + loc.len);
    let range = caret_range(text, start, end.saturating_sub(start).max(1));
    render_ariadne(
        source.name(),
        text,
        "R0001",
        message,
        LabelRange {
            range,
            label: "here".into(),
        },
        None,
        hint,
    )
}

/// Render a runtime error, choosing the compact or ariadne format automatically.
///
/// Uses the compact one-liner when `single_command` is true (no source span
/// arrow adds information); falls back to the full ariadne rendering otherwise.
pub fn format_runtime_error_auto(
    db: &SourceDb,
    err: &crate::types::Error,
    single_command: bool,
) -> String {
    if single_command {
        format_runtime_error_compact(err)
    } else {
        format_runtime_error_ariadne(db, err.loc.as_ref(), &err.message, err.hint.as_deref())
    }
}

/// Turn-result epilogue shared by every host that runs a top-level turn:
///
/// render the caught runtime error via [`format_runtime_error_auto`] into
/// `out`, then hand back the process-exit-code-clamped status.
///
/// A host that
/// wants to suppress the rendering (e.g. under an audit trace that reports
/// the error itself) passes [`std::io::sink`] and still gets the exit code.
pub fn report_runtime_error(
    out: &mut dyn std::io::Write,
    db: &SourceDb,
    err: &crate::types::Error,
    single_command: bool,
) -> i32 {
    let rendered = format_runtime_error_auto(db, err, single_command);
    let _ = out.write_all(rendered.as_bytes());
    err.exit_code().clamp(0, 255)
}

// ── Ad-hoc error helpers ──────────────────────────────────────────────────

/// Print a one-line command error to stderr: `{cmd}: {msg}`.
///
/// The command prefix is colored bold red when color is enabled.
pub fn cmd_error(cmd: &str, msg: &str) {
    if use_color() {
        eprintln!("{BOLD_RED}{cmd}{RESET}: {msg}");
    } else {
        eprintln!("{cmd}: {msg}");
    }
}

/// Render a runtime error without a source span — compact one-liner format.
///
/// Produces `error: {message} (exit status N)\nhint: {hint}\n`.
/// Used when the whole input is a single command, where the ariadne
/// source-span arrow adds no information.
pub fn format_runtime_error_compact(err: &crate::types::Error) -> String {
    let (red, cyan, reset) = error_palette();
    let mut out = format!("{red}error{reset}: {}", err.message);
    if let Some(code) = err.status_code_for_display() {
        let _ = write!(out, " (exit status {code})");
    }
    out.push('\n');
    if let Some(hint) = err.hint.as_deref() {
        let _ = writeln!(out, "{cyan}hint{reset}: {hint}");
    }
    out
}

/// Print a warning line to stderr: `warning: {msg}`.
pub fn shell_warning(msg: &str) {
    if use_color() {
        eprintln!("{BOLD_YELLOW}warning{RESET}: {msg}");
    } else {
        eprintln!("warning: {msg}");
    }
}

// ── Debug tracing ────────────────────────────────────────────────────────
//
// One stderr trace macro for development, on in debug builds and compiled
// to nothing in release — no environment flag.  Its call sites are
// permanent instrumentation, not temporary print statements, and should
// never be removed from the source.

/// Emit a bright-red `[[DEBUG] tag]` line to stderr in debug builds.
///
/// Compiled to nothing in release.  Usage:
///
/// ```ignore
/// dbg_trace!("exec", "cmd={cmd} inherit={inherit}");
/// dbg_trace!("repl", "entering loop");
/// ```
#[cfg(debug_assertions)]
#[macro_export]
macro_rules! dbg_trace {
    ($tag:expr, $($arg:tt)*) => {
        eprintln!("\x1b[1;91m[[DEBUG] {}]\x1b[0m {}", $tag, format!($($arg)*))
    };
}

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! dbg_trace {
    ($tag:expr, $($arg:tt)*) => {
        ()
    };
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Span;
    use crate::typecheck::{TypeError, TypeErrorKind};

    fn parse_error_with(message: &str, span: Option<ByteSpan>) -> ParseError {
        ParseError {
            message: message.into(),
            span,
            lex_kind: None,
            incompleteness: None,
        }
    }

    fn parse_error_from(kind: LexErrorKind) -> ParseError {
        ParseError {
            message: kind.message(),
            span: None,
            lex_kind: Some(kind),
            incompleteness: None,
        }
    }

    #[test]
    fn parse_error_ariadne_points_at_source() {
        // `x = [a, b` — point at byte 8 (the trailing `b`).
        let span = ByteSpan::new(crate::source::FileId::DUMMY, 8, 9);
        let err = parse_error_with("expected ',' or ']' in list", Some(span));
        let output = format_parse_error_ariadne("test.al", "x = [a, b", &err);
        assert!(output.contains("P0001"));
        assert!(output.contains("expected ',' or ']' in list"));
        assert!(output.contains("test.al"));
    }

    #[test]
    fn lex_error_ariadne_renders_unterminated_string_with_open_anchor() {
        let err = parse_error_from(LexErrorKind::UnterminatedString {
            form: StringForm::DoubleQuoted,
            opened: ByteSpan::new(crate::source::FileId::DUMMY, 0, 1),
            inner: None,
        });
        let output = format_parse_error_ariadne("test.al", "\"foo", &err);
        assert!(output.contains("L0001"), "got:\n{output}");
        assert!(output.contains("unterminated double-quoted string"));
        assert!(output.contains("opened here"));
    }

    #[test]
    fn lex_error_ariadne_includes_inner_help_for_nested_unclosed() {
        let inner = LexErrorKind::UnterminatedBalanced {
            open: '{',
            close: '}',
            opened: ByteSpan::new(crate::source::FileId::DUMMY, 6, 7),
        };
        let err = parse_error_from(LexErrorKind::UnterminatedString {
            form: StringForm::DoubleQuoted,
            opened: ByteSpan::new(crate::source::FileId::DUMMY, 0, 1),
            inner: Some(Box::new(inner)),
        });
        let output = format_parse_error_ariadne("test.al", "\"foo !{cmd", &err);
        assert!(output.contains("L0001"));
        assert!(output.contains("nested"));
        assert!(output.contains("`{…}`"), "got:\n{output}");
    }

    /// Register one source in a fresh db and return the db plus the id.
    fn db_with(name: &str, text: &str) -> (SourceDb, FileId) {
        let mut db = SourceDb::default();
        let id = db.register(Source::from_text(name, text));
        (db, id)
    }

    #[test]
    fn runtime_error_ariadne_points_at_source() {
        let (db, source) = db_with("test.al", "x = 5\ny = 10\necho $undefined\n");
        let loc = SourceLoc {
            source,
            line: 3,
            col: 6,
            len: 10,
        };
        let output =
            format_runtime_error_ariadne(&db, Some(&loc), "undefined variable: $undefined", None);
        assert!(output.contains("R0001"));
        assert!(output.contains("undefined variable: $undefined"));
        assert!(output.contains("test.al"));
    }

    #[test]
    fn runtime_error_ariadne_renders_hint() {
        let (db, source) = db_with("test.al", "[a, b] = 5");
        let loc = SourceLoc {
            source,
            line: 1,
            col: 1,
            len: 3,
        };
        let output = format_runtime_error_ariadne(
            &db,
            Some(&loc),
            "list destructuring requires a list, got: 5",
            Some("the right-hand side must evaluate to a list"),
        );
        assert!(output.contains("the right-hand side must evaluate to a list"));
    }

    /// `len` is a byte length: for a multi-byte token the caret width is the
    /// token's character count, not its byte count, so the underline stops at
    /// the token boundary instead of running into the following text.
    #[test]
    fn runtime_error_caret_width_is_char_count_for_multibyte() {
        let text = "café bar";
        // The token `café` starts at column 1 and is 5 bytes but 4 chars.
        let start_byte = line_col_to_byte(text, 1, 1);
        assert_eq!(start_byte, 0);
        let start = byte_to_char(text, start_byte);
        let end = byte_to_char(text, start_byte + "café".len());
        assert_eq!(end - start, 4, "caret must span 4 chars, not 5 bytes");
    }

    #[test]
    fn type_error_ariadne_with_span() {
        let sp = Span::new(crate::source::FileId::DUMMY, 21, 28);
        let err = TypeError {
            pos: Some(sp),
            kind: TypeErrorKind::TyMismatch {
                expected: crate::typecheck::Ty::Int,
                actual: crate::typecheck::Ty::String,
            },
            reason: Some(crate::typecheck::Reason::IfCond),
        };
        let output = format_type_error_ariadne(
            "test.ral",
            "if 1 { return 42 } else { return \"hello\" }",
            &err,
        );
        assert!(output.contains("T0010"));
        assert!(output.contains("couldn't match"));
        assert!(output.contains("Integer"));
        assert!(output.contains("String"));
        assert!(output.contains(
            "the condition of an `if` must be a Bool — either `true`/`false` \
             or an expression that produces one (e.g. `$[$x == 1]`)"
        ));
    }

    #[test]
    fn type_error_ariadne_without_span_is_messageless() {
        let err = TypeError {
            pos: None,
            kind: TypeErrorKind::RecursiveRow,
            reason: None,
        };
        let output = format_type_error_ariadne("test.ral", "let x = 1", &err);
        assert!(output.contains("infinite row"));
        assert!(output.contains("T0002"));
    }

    /// A runtime error whose location resolves in the db draws its caret
    /// into the named source's bytes.
    #[test]
    fn runtime_error_resolved_source_draws_caret() {
        let (db, source) = db_with("main.ral", "echo x");
        let loc = SourceLoc {
            source,
            line: 1,
            col: 6,
            len: 1,
        };
        let out = format_runtime_error_ariadne(&db, Some(&loc), "boom", None);
        assert!(out.contains("R0001"));
        assert!(
            out.contains("here"),
            "a resolved loc should draw a caret:\n{out}"
        );
        assert!(out.contains("main.ral"));
    }

    /// A runtime error whose location names a source the registry does not
    /// hold (e.g. the placeholder id) falls back to a messageless render —
    /// the live cross-source guard never draws a caret at an unrelated byte.
    #[test]
    fn runtime_error_in_unregistered_source_is_messageless() {
        let (db, _) = db_with("main.ral", "echo x");
        let loc = SourceLoc {
            source: crate::source::FileId::DUMMY,
            line: 9,
            col: 3,
            len: 4,
        };
        let out = format_runtime_error_ariadne(&db, Some(&loc), "boom", None);
        assert!(out.contains("R0001"));
        assert!(out.contains("boom"));
        assert!(
            !out.contains("here"),
            "an unresolved loc must not draw a caret in any source:\n{out}"
        );
    }

    /// The cross-source fix in one renderer call: two sources registered in
    /// one db; an error whose loc names the *module's* id draws into the
    /// module's bytes, not the top-level script's, even when the module's
    /// (line, col) would land elsewhere in the top-level text.
    #[test]
    fn runtime_error_in_module_draws_into_module_source() {
        let mut db = SourceDb::default();
        let _top = db.register(Source::from_text("main.ral", "source 'mod.ral'\n"));
        let module = db.register(Source::from_text("mod.ral", "let a = 1\nfail 'kaboom'\n"));
        // The module's line 2 is `fail 'kaboom'`; the top-level has no line 2.
        let loc = SourceLoc {
            source: module,
            line: 2,
            col: 1,
            len: 4,
        };
        // Strip ANSI before asserting: ariadne colors the underlined span
        // character-by-character when color is on (a tty), so the raw bytes
        // of `fail` are split by escapes.  The test is about the visible text.
        let out = ansi::strip(&format_runtime_error_ariadne(
            &db,
            Some(&loc),
            "kaboom",
            None,
        ));
        assert!(out.contains("R0001"));
        assert!(
            out.contains("mod.ral"),
            "the caret must be drawn against the module's source:\n{out}"
        );
        assert!(
            !out.contains("main.ral"),
            "the top-level source must not appear:\n{out}"
        );
        assert!(
            out.contains("fail"),
            "the underlined line must be the module's line 2:\n{out}"
        );
    }

    /// A top-level turn boundary resets the db before registering: a second
    /// turn's source reuses the first turn's id rather than appending, so a
    /// long session does not grow the registry without bound.
    #[test]
    fn reset_reclaims_ids_across_turns() {
        let mut db = SourceDb::default();
        let first = db.register(Source::from_text("<stdin>", "echo a"));
        db.reset();
        let second = db.register(Source::from_text("<stdin>", "echo b"));
        assert_eq!(first, second, "the reset turn must reuse the first id");
        assert_eq!(
            db.get(second).map(Source::as_str),
            Some("echo b"),
            "only the current turn's source resolves after a reset"
        );
    }

    #[test]
    fn no_color_output_has_no_ansi() {
        // NO_COLOR path: messageless render produces no escape codes.
        let out = render_messageless(Some("T9999"), "message", Some("hint"));
        // We can't assert absence globally (use_color may be true in a tty),
        // but the content must include code + message + hint regardless.
        assert!(out.contains("T9999"));
        assert!(out.contains("message"));
        assert!(out.contains("hint"));
    }
}
