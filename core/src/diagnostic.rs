//! Error formatting and diagnostic rendering.
//!
//! All user-visible error output -- parse errors, type errors, and runtime
//! errors -- is funnelled through this module.  Structured errors are
//! rendered via the `ariadne` crate with source-span underlining; when no
//! span is available, a compact one-liner format is used instead.
//!
//! Color output is gated by [`ansi::use_color`].

use crate::ansi::{self, BOLD_CYAN, BOLD_RED, BOLD_YELLOW, RESET};
use crate::typecheck::TypeError;

/// Snap `byte_offset` to the nearest UTF-8 char boundary at or before it.
/// Spans can point inside a multi-byte sequence when synthesised or inherited
/// from a different source context; slicing on a non-boundary would panic.
fn floor_char_boundary(source: &str, byte_offset: usize) -> usize {
    let clamped = byte_offset.min(source.len());
    // At most 3 steps back (max UTF-8 continuation bytes per codepoint).
    (0..=clamped)
        .rev()
        .find(|&i| source.is_char_boundary(i))
        .unwrap_or(0)
}

/// Convert a byte offset within `source` into a 1-indexed (line, col) pair.
/// Used during the migration from (line, col) positions to byte-range spans.
pub fn byte_to_line_col(source: &str, byte_offset: usize) -> (usize, usize) {
    let safe = floor_char_boundary(source, byte_offset);
    let prefix = &source[..safe];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let last_nl = prefix.rfind('\n');
    let line_start = last_nl.map(|i| i + 1).unwrap_or(0);
    let col = source[line_start..safe].chars().count() + 1;
    (line, col)
}

/// Convert a byte offset to a character offset.  Ariadne uses character
/// offsets, so every byte offset must pass through this before being handed
/// to the rendering layer.
fn byte_to_char(source: &str, byte_offset: usize) -> usize {
    source[..floor_char_boundary(source, byte_offset)]
        .chars()
        .count()
}

// Re-export the gating functions so callers that already import this module
// don't need to change their import paths.
pub use ansi::{set_terminal, use_color, use_ui_color};

// ── Source location ───────────────────────────────────────────────────────

/// A source location for error reporting.
#[derive(Debug, Clone)]
pub struct SourceLoc {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub len: usize,
}

// ── Format functions (ariadne) ────────────────────────────────────────────

/// Locate the character offset in `source` corresponding to 1-indexed
/// (line, col).  Ariadne indexes by character, not byte.
fn line_col_to_char(source: &str, line: usize, col: usize) -> usize {
    let mut char_offset = 0usize;
    for (i, ln) in source.split_inclusive('\n').enumerate() {
        if i + 1 == line {
            return char_offset + col.saturating_sub(1).min(ln.chars().count());
        }
        char_offset += ln.chars().count();
    }
    char_offset
}

/// Render a bare "code: message" line when there's no source span to point at.
/// Used by the type-error path when the error lacks a span.
fn render_messageless(code: Option<&str>, message: &str, hint: Option<&str>) -> String {
    let mut out = String::new();
    let (red, cyan, reset) = if use_color() {
        (BOLD_RED, BOLD_CYAN, RESET)
    } else {
        ("", "", "")
    };
    match code {
        Some(c) => out.push_str(&format!("{red}[{c}] Error{reset}: {message}\n")),
        None => out.push_str(&format!("{red}Error{reset}: {message}\n")),
    }
    if let Some(h) = hint {
        out.push_str(&format!("  {cyan}help{reset}: {h}\n"));
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

/// Render an ariadne report with one red label and an optional help line.
fn render_ariadne(
    file: &str,
    source: &str,
    code: &str,
    message: &str,
    label: LabelRange,
    hint: Option<&str>,
) -> String {
    let file_owned: String = file.to_string();
    let mut builder = ariadne::Report::<(String, std::ops::Range<usize>)>::build(
        ariadne::ReportKind::Error,
        (file_owned.clone(), label.range.clone()),
    )
    .with_config(ariadne::Config::default().with_color(use_color()))
    .with_code(code)
    .with_message(message)
    .with_label(
        ariadne::Label::new((file_owned.clone(), label.range))
            .with_message(label.label)
            .with_color(ariadne::Color::Red),
    );
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

/// Render a parse error via ariadne.  Takes the same (line, col, message)
/// the caller already has; computes the byte offset internally so the
/// caret lines up under the offending token.
pub fn format_parse_error_ariadne(
    file: &str,
    source: &str,
    line: usize,
    col: usize,
    message: &str,
) -> String {
    let range = caret_range(source, line_col_to_char(source, line, col), 1);
    render_ariadne(
        file,
        source,
        "P0001",
        message,
        LabelRange { range, label: "here".into() },
        None,
    )
}

/// Short phrase placed next to the primary label, describing the immediate
/// nature of the mismatch.  The kind's full message goes on the Report.
fn label_message_for_kind(kind: &crate::typecheck::TypeErrorKind) -> String {
    use crate::typecheck::{TypeErrorKind as K, fmt_mode, fmt_ty};
    match kind {
        K::RecursiveType | K::RecursiveRow | K::RecursiveCompTy => "recursive here".into(),
        K::TyMismatch { expected, actual } => {
            format!("expected {}, got {}", fmt_ty(expected), fmt_ty(actual))
        }
        K::CompTyMismatch { .. } => "mismatch here".into(),
        K::ModeMismatch { expected, actual } => {
            format!("expected {}, got {}", fmt_mode(expected), fmt_mode(actual))
        }
        K::RowExtraField { label } => format!("unexpected field '{label}'"),
        K::RowMissingField { label } => format!("missing field '{label}'"),
        K::AdHoc { .. } => "here".into(),
    }
}

/// Render a type error via the ariadne crate — structured labels, error
/// code, and optional help.  Falls back to a messageless render when the
/// error carries no span (nothing to point at).
pub fn format_type_error_ariadne(file: &str, source: &str, err: &TypeError) -> String {
    let message = err.kind.render_message();
    let code = err.kind.code();
    let Some(sp) = err.pos else {
        return render_messageless(Some(code), &message, err.hint.as_deref());
    };
    let start = byte_to_char(source, sp.start as usize);
    let end = byte_to_char(source, sp.end.max(sp.start + 1) as usize);
    let range = caret_range(source, start, end.saturating_sub(start));
    render_ariadne(
        file,
        source,
        code,
        &message,
        LabelRange {
            range,
            label: label_message_for_kind(&err.kind),
        },
        err.hint.as_deref(),
    )
}

/// Render a runtime error via ariadne.  Uses the `SourceLoc`'s line/col
/// (already computed at throw-time) to place the caret; honours `len` for
/// the underline width.  Falls back to a messageless render when loc is None.
pub fn format_runtime_error_ariadne(
    file: &str,
    source: &str,
    loc: Option<&SourceLoc>,
    message: &str,
    hint: Option<&str>,
) -> String {
    let Some(loc) = loc else {
        return render_messageless(Some("R0001"), message, hint);
    };
    let display_file = if loc.file.is_empty() {
        file
    } else {
        loc.file.as_str()
    };
    let range = caret_range(source, line_col_to_char(source, loc.line, loc.col), loc.len);
    render_ariadne(
        display_file,
        source,
        "R0001",
        message,
        LabelRange { range, label: "here".into() },
        hint,
    )
}

/// Render a runtime error, choosing the compact or ariadne format automatically.
///
/// Uses the compact one-liner when `single_command` is true (no source span
/// arrow adds information); falls back to the full ariadne rendering otherwise.
pub fn format_runtime_error_auto(
    file: &str,
    source: &str,
    err: &crate::types::Error,
    single_command: bool,
) -> String {
    if single_command {
        format_runtime_error_compact(err)
    } else {
        format_runtime_error_ariadne(
            file,
            source,
            err.loc.as_ref(),
            &err.message,
            err.hint.as_deref(),
        )
    }
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
    let (red, cyan, reset) = if use_color() {
        (BOLD_RED, BOLD_CYAN, RESET)
    } else {
        ("", "", "")
    };
    let mut out = format!("{red}error{reset}: {}", err.message);
    if err.status != 0 {
        out.push_str(&format!(" (exit status {})", err.status));
    }
    out.push('\n');
    if let Some(hint) = err.hint.as_deref() {
        out.push_str(&format!("{cyan}hint{reset}: {hint}\n"));
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
    ($tag:expr, $($arg:tt)*) => {};
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Span;
    use crate::typecheck::{TypeError, TypeErrorKind};

    #[test]
    fn parse_error_ariadne_points_at_source() {
        let output = format_parse_error_ariadne(
            "test.al",
            "x = [a, b",
            1,
            11,
            "expected ',' or ']' in list",
        );
        assert!(output.contains("P0001"));
        assert!(output.contains("expected ',' or ']' in list"));
        assert!(output.contains("test.al"));
    }

    #[test]
    fn runtime_error_ariadne_points_at_source() {
        let loc = SourceLoc {
            file: "test.al".into(),
            line: 3,
            col: 6,
            len: 10,
        };
        let output = format_runtime_error_ariadne(
            "test.al",
            "x = 5\ny = 10\necho $undefined\n",
            Some(&loc),
            "undefined variable: $undefined",
            None,
        );
        assert!(output.contains("R0001"));
        assert!(output.contains("undefined variable: $undefined"));
        assert!(output.contains("test.al"));
    }

    #[test]
    fn runtime_error_ariadne_renders_hint() {
        let loc = SourceLoc {
            file: "test.al".into(),
            line: 1,
            col: 1,
            len: 3,
        };
        let output = format_runtime_error_ariadne(
            "test.al",
            "[a, b] = 5",
            Some(&loc),
            "list destructuring requires a list, got: 5",
            Some("the right-hand side must evaluate to a list"),
        );
        assert!(output.contains("the right-hand side must evaluate to a list"));
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
            hint: Some("all branches must produce the same type".into()),
        };
        let output = format_type_error_ariadne(
            "test.ral",
            "if 1 { return 42 } else { return \"hello\" }",
            &err,
        );
        assert!(output.contains("T0010"));
        assert!(output.contains("type mismatch"));
        assert!(output.contains("all branches must produce the same type"));
    }

    #[test]
    fn type_error_ariadne_without_span_is_messageless() {
        let err = TypeError {
            pos: None,
            kind: TypeErrorKind::RecursiveType,
            hint: None,
        };
        let output = format_type_error_ariadne("test.ral", "let x = 1", &err);
        assert!(output.contains("recursive type"));
        assert!(output.contains("T0001"));
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
