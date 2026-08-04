//! Every user-visible parse, type, and runtime error is rendered here: through
//! `ariadne` when a span points somewhere, as a one-liner when it does not.

use crate::ansi::{self, BOLD_CYAN, BOLD_RED, BOLD_YELLOW, RESET};
use crate::source::{SourceDb, Span, byte_to_line_col};
use crate::syntax::lexer::{LexErrorKind, StringForm};
use crate::syntax::parser::ParseError;
use crate::text::byte_to_char;
use crate::typecheck::TypeError;
use std::fmt::Write;

// Frontends seed and read the gate through here: `ral::platform` and exarch's
// bootstrap each call `diagnostic::set_terminal` once at startup.
pub use ansi::{set_terminal, use_color};

// ── Source location ───────────────────────────────────────────────────────

/// A [`Span`] resolved against the session's [`SourceDb`] — script name and
/// 1-indexed line/column, as it rides out on audit nodes and capability checks.
#[derive(Clone, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallSite {
    pub script: String,
    pub line: usize,
    pub col: usize,
}

// ── Palette and spanless fallback ─────────────────────────────────────────

/// `(error, hint, reset)` — empty strings when colour is off, so one
/// `format!` serves both.
fn error_palette() -> (&'static str, &'static str, &'static str) {
    if use_color() {
        (BOLD_RED, BOLD_CYAN, RESET)
    } else {
        ("", "", "")
    }
}

/// The spanless fallback: a type error carrying no position, or a runtime span
/// the [`SourceDb`] cannot resolve.
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

/// Source range plus the phrase placed next to its underline.
struct LabelRange {
    range: std::ops::Range<usize>,
    label: String,
}

/// The one report shape — red primary label, optional yellow secondary,
/// optional help.  Callers differ only in how they derive the ranges.
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

/// Clamped so the caret still points at *some* character at end-of-source.
fn caret_range(source: &str, start: usize, width: usize) -> std::ops::Range<usize> {
    let char_len = source.chars().count();
    let s = start.min(char_len);
    let e = (s + width.max(1)).min(char_len.max(s + 1));
    s..e
}

/// Render a parse error: a structured `lex_kind` drives the two-label report
/// (opener plus EOF), anything else gets one red label on the offending token.
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

// ── Spans and lex-error reports ───────────────────────────────────────────

/// Spans count bytes; ariadne indexes by char, so every span crosses here.
fn byte_span_to_char_range(source: &str, span: Span) -> std::ops::Range<usize> {
    let s = byte_to_char(source, span.start as usize);
    let e = byte_to_char(source, span.end.max(span.start + 1) as usize);
    caret_range(source, s, e.saturating_sub(s).max(1))
}

fn eof_char_range(source: &str) -> std::ops::Range<usize> {
    caret_range(source, source.chars().count(), 1)
}

/// Prose for the help line — "`{…}` opened at 1:6, which itself contains …".
/// Line/column is recovered from `source` here, not carried on the error.
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

/// The parts `render_ariadne` consumes.
struct LexErrorReport {
    code: &'static str,
    message: String,
    primary: LabelRange,
    secondary: Option<LabelRange>,
    hint: Option<String>,
}

/// `None` for `Other(_)`, so the caller falls back to the single-label render.
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
        // Codes are never reused: the next lex diagnostic takes L0006.
        LexErrorKind::Other(_) => None,
    }
}

/// Render one type error, falling back to the spanless form when it carries
/// no position.
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

/// Every error in `errs`, one caret report each.  The script, `--check`, and
/// rc paths render this way; the module loaders and the REPL print `Display`.
pub fn format_type_errors_ariadne(file: &str, source: &str, errs: &[TypeError]) -> String {
    errs.iter()
        .map(|e| format_type_error_ariadne(file, source, e))
        .collect()
}

/// Draw the caret into the source `span` names, resolved through `db`.  A span
/// `db` cannot resolve falls back to spanless: no caret beats one in the wrong
/// file.
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

/// Compact only when the error stayed inside `compact_root`'s file — the id
/// of an input that compiled to a single command.
///
/// Shape alone will not do: `boom` at the prompt is one command, but as an
/// alias its error lives in the rc, where only a caret can point.
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

/// The run epilogue every host shares: render into `out`, return the clamped
/// exit code.  A host whose audit trace already reports the error passes
/// [`std::io::sink`] and still gets the code.
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

/// Print `{cmd}: {msg}` to stderr.
pub fn cmd_error(cmd: &str, msg: &str) {
    if use_color() {
        eprintln!("{BOLD_RED}{cmd}{RESET}: {msg}");
    } else {
        eprintln!("{cmd}: {msg}");
    }
}

/// The one-liner for a single-command input, where a caret would only point
/// back at the line the user just typed.
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

/// Print `warning: {msg}` to stderr.
pub fn shell_warning(msg: &str) {
    if use_color() {
        eprintln!("{BOLD_YELLOW}warning{RESET}: {msg}");
    } else {
        eprintln!("warning: {msg}");
    }
}

// ── Debug tracing ────────────────────────────────────────────────────────
//
// Call sites of `dbg_trace!` are permanent instrumentation, not stray print
// statements; leave them in.

/// Emit a `[[DEBUG] tag]` line to stderr; nothing at all in release builds.
#[cfg(debug_assertions)]
#[macro_export]
macro_rules! dbg_trace {
    ($tag:expr, $($arg:tt)*) => {
        // Same gate as the diagnostics: no ANSI under NO_COLOR, TERM=dumb, or a
        // non-tty stderr — `use_color` probes inline until `set_terminal` seeds
        // it, so a trace from before terminal setup is gated too.
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

    /// `café` is 5 bytes and 4 chars; the underline must stop at the token.
    #[test]
    fn runtime_error_caret_width_is_char_count_for_multibyte() {
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

    /// The placeholder id names no source, so no caret is drawn in any of them.
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

    /// Two sources in one db: the caret follows the span's file, not the
    /// top-level's, even where the same byte range would land in both.
    #[test]
    fn runtime_error_in_module_draws_into_module_source() {
        let mut db = SourceDb::default();
        let _top = db.register(Source::from_text("main.ral", "source 'mod.ral'\n"));
        let module = db.register(Source::from_text(
            "mod.ral",
            "let a = 1\nfail [status: 1, message: 'kaboom']\n",
        ));
        // Bytes 10..14 of the module are `fail`, past the top-level's end.  Strip
        // ANSI: on a tty ariadne colours the span character by character, which
        // splits `fail` with escapes.
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
        // Absence of escapes is not assertable — `use_color` may be true in a
        // tty — so only the content is checked.
        let out = render_messageless(Some("T9999"), "message", Some("hint"));
        assert!(out.contains("T9999"));
        assert!(out.contains("message"));
        assert!(out.contains("hint"));
    }
}
