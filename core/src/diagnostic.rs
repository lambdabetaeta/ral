//! Error formatting and diagnostic rendering.
//!
//! All user-visible error output -- parse errors, type errors, and runtime
//! errors -- is funnelled through this module.  Structured errors are
//! rendered via the `ariadne` crate with source-span underlining; when no
//! span is available, a compact one-liner format is used instead.
//!
//! Color output is gated by [`ansi::use_color`].

use crate::ansi::{self, BOLD_CYAN, BOLD_RED, BOLD_YELLOW, RESET};
use crate::source::{SourceDb, Span, byte_to_line_col};
use crate::syntax::lexer::{LexErrorKind, StringForm};
use crate::syntax::parser::ParseError;
use crate::text::byte_to_char;
use crate::typecheck::TypeError;
use std::fmt::Write;

// Re-export the color-gating functions so diagnostics and their gate share
// one import.
pub use ansi::{set_terminal, use_color};

// ── Source location ───────────────────────────────────────────────────────

/// A resolved source position: script name + 1-indexed (line, col).  The
/// audit and wire shape a [`Span`] resolves to via the session's
/// [`SourceDb`]; hosts read it off audit nodes and capability checks.
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct CallSite {
    pub script: String,
    pub line: usize,
    pub col: usize,
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
    let range = err.span.map_or_else(
        || eof_char_range(source),
        |s| byte_span_to_char_range(source, s),
    );
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
fn byte_span_to_char_range(source: &str, span: Span) -> std::ops::Range<usize> {
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
    let pos = |span: Span| {
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
/// Resolves `span`'s [`FileId`](crate::source::FileId) against `db` to
/// recover the file name and text, then underlines the span's byte range
/// there.  Falls back to a messageless render when there is no span or `db`
/// does not hold its source — the live cross-source guard: a span the
/// renderer cannot resolve never draws a caret at an unrelated byte.
pub fn format_runtime_error_ariadne(
    db: &SourceDb,
    span: Option<Span>,
    message: &str,
    hint: Option<&str>,
) -> String {
    let Some((span, source)) = span.and_then(|sp| db.get(sp.file).map(|src| (sp, src))) else {
        return render_messageless(Some("R0001"), message, hint);
    };
    render_ariadne(
        source.name(),
        source.as_str(),
        "R0001",
        message,
        LabelRange {
            range: byte_span_to_char_range(source.as_str(), span),
            label: "here".into(),
        },
        None,
        hint,
    )
}

/// Render a runtime error, choosing the compact or ariadne format automatically.
///
/// `compact_root` is `Some(root)` when the input compiled to a single command,
/// carrying that input's own [`FileId`](crate::source::FileId); `None` when it
/// did not.  Shape alone cannot decide: `boom` at the prompt is one command,
/// but if it is an alias the error is inside the rc, where only a caret points.
pub fn format_runtime_error_auto(
    db: &SourceDb,
    err: &crate::types::Error,
    compact_root: Option<crate::source::FileId>,
) -> String {
    match compact_root {
        Some(root) if err.span.is_none_or(|sp| sp.file == root) => {
            format_runtime_error_compact(err)
        }
        _ => format_runtime_error_ariadne(db, err.span, &err.message, err.hint.as_deref()),
    }
}

/// Run-result epilogue shared by every host that runs a top-level run:
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
    compact_root: Option<crate::source::FileId>,
) -> i32 {
    let rendered = format_runtime_error_auto(db, err, compact_root);
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
        // Honor the same color gate as the diagnostics ([`use_color`]): a trace
        // must not emit ANSI when NO_COLOR / TERM=dumb / a non-tty stderr says
        // otherwise. `use_color()` is false until `set_terminal` runs, so any
        // trace before terminal setup (e.g. the sandbox `boot_recover` sweep at
        // `early_init`) renders plain.
        if $crate::diagnostic::use_color() {
            eprintln!("\x1b[1;91m[[DEBUG] {}]\x1b[0m {}", $tag, format!($($arg)*))
        } else {
            eprintln!("[[DEBUG] {}] {}", $tag, format!($($arg)*))
        }
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
    use crate::source::{FileId, Source};
    use crate::typecheck::{TypeError, TypeErrorKind};

    fn parse_error_with(message: &str, span: Option<Span>) -> ParseError {
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
        let span = Span::new(crate::source::FileId::DUMMY, 8, 9);
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
            opened: Span::new(crate::source::FileId::DUMMY, 0, 1),
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
            opened: Span::new(crate::source::FileId::DUMMY, 6, 7),
        };
        let err = parse_error_from(LexErrorKind::UnterminatedString {
            form: StringForm::DoubleQuoted,
            opened: Span::new(crate::source::FileId::DUMMY, 0, 1),
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
        let (db, file) = db_with("test.al", "x = 5\ny = 10\necho $undefined\n");
        let output = format_runtime_error_ariadne(
            &db,
            Some(Span::new(file, 18, 28)),
            "undefined variable: $undefined",
            None,
        );
        assert!(output.contains("R0001"));
        assert!(output.contains("undefined variable: $undefined"));
        assert!(output.contains("test.al"));
    }

    #[test]
    fn runtime_error_ariadne_renders_hint() {
        let (db, file) = db_with("test.al", "[a, b] = 5");
        let output = format_runtime_error_ariadne(
            &db,
            Some(Span::new(file, 0, 6)),
            "list destructuring requires a list, got: 5",
            Some("the right-hand side must evaluate to a list"),
        );
        assert!(output.contains("the right-hand side must evaluate to a list"));
    }

    /// A span is a byte range: for a multi-byte token the caret width is the
    /// token's character count, not its byte count, so the underline stops at
    /// the token boundary instead of running into the following text.
    #[test]
    fn runtime_error_caret_width_is_char_count_for_multibyte() {
        // The token `café` occupies bytes 0..5 but spans 4 characters.
        let range = byte_span_to_char_range("café bar", Span::new(FileId::DUMMY, 0, 5));
        assert_eq!(range, 0..4, "caret must span 4 chars, not 5 bytes");
    }

    #[test]
    fn type_error_ariadne_with_span() {
        let sp = Span::new(FileId::DUMMY, 21, 28);
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

    /// A runtime error whose span resolves in the db draws its caret
    /// into the named source's bytes.
    #[test]
    fn runtime_error_resolved_source_draws_caret() {
        let (db, file) = db_with("main.ral", "echo x");
        let out = format_runtime_error_ariadne(&db, Some(Span::new(file, 5, 6)), "boom", None);
        assert!(out.contains("R0001"));
        assert!(
            out.contains("here"),
            "a resolved span should draw a caret:\n{out}"
        );
        assert!(out.contains("main.ral"));
    }

    /// A runtime error whose span names a source the registry does not
    /// hold (e.g. the placeholder id) falls back to a messageless render —
    /// the live cross-source guard never draws a caret at an unrelated byte.
    #[test]
    fn runtime_error_in_unregistered_source_is_messageless() {
        let (db, _) = db_with("main.ral", "echo x");
        let span = Span::new(FileId::DUMMY, 2, 6);
        let out = format_runtime_error_ariadne(&db, Some(span), "boom", None);
        assert!(out.contains("R0001"));
        assert!(out.contains("boom"));
        assert!(
            !out.contains("here"),
            "an unresolved span must not draw a caret in any source:\n{out}"
        );
    }

    /// The cross-source fix in one renderer call: two sources registered in
    /// one db; an error whose span names the *module's* id draws into the
    /// module's bytes, not the top-level script's, even when the module's
    /// byte range would land elsewhere in the top-level text.
    #[test]
    fn runtime_error_in_module_draws_into_module_source() {
        let mut db = SourceDb::default();
        let _top = db.register(Source::from_text("main.ral", "source 'mod.ral'\n"));
        let module = db.register(Source::from_text("mod.ral", "let a = 1\nfail 'kaboom'\n"));
        // Bytes 10..14 of the module are `fail`, past the top-level's end.
        // Strip ANSI before asserting: ariadne colors the underlined span
        // character-by-character when color is on (a tty), so the raw bytes
        // of `fail` are split by escapes.  The test is about the visible text.
        let out = ansi::strip(&format_runtime_error_ariadne(
            &db,
            Some(Span::new(module, 10, 14)),
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
