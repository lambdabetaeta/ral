//! Error formatting and diagnostic rendering.
//!
//! All user-visible error output -- parse errors, type errors, and runtime
//! errors -- is funnelled through this module.  Structured errors are
//! rendered via the `ariadne` crate with source-span underlining; when no
//! span is available, a compact one-liner format is used instead.
//!
//! Color output is gated by [`ansi::use_color`].

use crate::ansi::{self, BOLD_CYAN, BOLD_RED, BOLD_YELLOW, RESET};
use crate::source::{FileId, Span as ByteSpan};
use crate::syntax::lexer::{LexErrorKind, StringForm};
use crate::syntax::parser::ParseError;
use crate::typecheck::TypeError;
use std::fmt::Write;
use std::sync::Arc;

use crate::text::floor_char_boundary;

/// Source text bundled with a precomputed line-start index.
///
/// Binary search
/// over the index resolves a `(byte_offset → line, col)` lookup in
/// O(log lines), independent of file size; `eval_comp` recomputes `Location`
/// from a span on every node it visits, so the per-lookup cost is on a hot
/// path.
///
/// Built once when the source is loaded; `Arc<[u32]>` makes Location
/// clones (which happen every closure call) refcount-bumps rather
/// than copies.
#[derive(Clone, Debug)]
pub struct Source {
    /// Display name of the source — a script path, `<stdin>`, or a loaded
    /// module's virtual path.  Carried so a runtime error rendered from a
    /// [`SourceDb`] names the file the caret points into.
    name: Arc<str>,
    text: Arc<str>,
    /// Sorted byte offsets where each line starts.  `line_starts[0] == 0`;
    /// thereafter, `line_starts[i]` is the byte index immediately after the
    /// `i`-th newline.  Length is therefore one greater than the newline
    /// count in `text`.
    line_starts: Arc<[u32]>,
}

impl Source {
    /// Wrap `text` under display `name`, building the line-start index in
    /// one pass.
    pub fn new(name: Arc<str>, text: Arc<str>) -> Self {
        let mut starts: Vec<u32> =
            Vec::with_capacity(text.bytes().filter(|&b| b == b'\n').count() + 1);
        starts.push(0);
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "byte offset into a source that fits the u32 span system (< 4 GiB, compiler-standard)"
                )]
                starts.push((i + 1) as u32);
            }
        }
        Self {
            name,
            text,
            line_starts: starts.into(),
        }
    }

    /// Convenience: wrap `text` under `name` by allocating and indexing.
    pub fn from_text(name: &str, text: &str) -> Self {
        Self::new(Arc::from(name), Arc::from(text))
    }

    /// Borrow the source's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrow the underlying source text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Convert a byte offset into a 1-indexed (line, col) pair using the
    /// precomputed index.  O(log lines) for the line lookup, plus one
    /// `chars().count()` over the (typically short) current line for the
    /// column.
    pub fn byte_to_line_col(&self, byte_offset: usize) -> (usize, usize) {
        let safe = floor_char_boundary(&self.text, byte_offset);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "byte offset into a source that fits the u32 span system (< 4 GiB, compiler-standard)"
        )]
        let target = safe as u32;
        // Largest i such that line_starts[i] <= target.  partition_point
        // returns the first i where the predicate is false; subtract one.
        let line_idx = self
            .line_starts
            .partition_point(|&start| start <= target)
            .saturating_sub(1);
        let line_start = self.line_starts[line_idx] as usize;
        let line = line_idx + 1;
        let col = self.text[line_start..safe].chars().count() + 1;
        (line, col)
    }
}

/// Registry of every source text the current turn has loaded, keyed by
/// [`FileId`].
///
/// A [`SourceLoc`] carries the `FileId` of the source whose
/// `line`/`col` index it holds, and the runtime renderer resolves that id
/// here so a `source`d module's error draws its caret into the module's own
/// bytes rather than the top-level script's.
///
/// `Arc`-shared so the per-closure `Location` clone is a refcount bump.
/// Within a turn the top-level source and each module it loads each register
/// once; [`reset`](Self::reset) at the next turn boundary reclaims them.
#[derive(Clone, Debug, Default)]
pub struct SourceDb {
    sources: Arc<Vec<Source>>,
}

impl SourceDb {
    /// Register `source`, returning the [`FileId`] that resolves to it.
    pub fn register(&mut self, source: Source) -> FileId {
        let sources = Arc::make_mut(&mut self.sources);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "FileId is u32; a turn registers a handful of sources, far below 2^32"
        )]
        let id = FileId(sources.len() as u32);
        sources.push(source);
        id
    }

    /// Drop every registered source, returning the registry to empty so the
    /// next [`register`](Self::register) hands out [`FileId`] with index `0` again.
    /// Called at each top-level turn boundary so a long interactive session
    /// reclaims the prior turn's sources instead of growing without bound.
    pub fn reset(&mut self) {
        Arc::make_mut(&mut self.sources).clear();
    }

    /// Resolve `id` to its registered [`Source`], or `None` when the id is
    /// the placeholder [`FileId::DUMMY`] or names a source this registry
    /// does not hold.
    pub fn get(&self, id: FileId) -> Option<&Source> {
        self.sources.get(id.0 as usize)
    }

    /// Peek the [`FileId`] the next [`register`](Self::register) call will
    /// mint, without registering anything. Lets a caller stamp the id onto
    /// a program's spans *before* the source it names is itself registered
    /// — sound exactly when nothing else registers a source into this
    /// registry between the peek and that later registration.
    pub fn next_id(&self) -> FileId {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "FileId is u32; a turn registers a handful of sources, far below 2^32"
        )]
        FileId(self.sources.len() as u32)
    }
}

/// Convert a byte offset within `source` into a 1-indexed (line, col) pair.
///
/// Linear-scan version retained for one-off callers that do not have a
/// cached `Source` to hand; hot paths should build a [`Source`] once and
/// call [`Source::byte_to_line_col`] instead.
pub fn byte_to_line_col(source: &str, byte_offset: usize) -> (usize, usize) {
    let safe = floor_char_boundary(source, byte_offset);
    let prefix = &source[..safe];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let last_nl = prefix.rfind('\n');
    let line_start = last_nl.map_or(0, |i| i + 1);
    let col = source[line_start..safe].chars().count() + 1;
    (line, col)
}

/// Convert a byte offset to a character offset.  Ariadne uses character
/// offsets, so every byte offset must pass through this before being handed
/// to the rendering layer.
pub fn byte_to_char(source: &str, byte_offset: usize) -> usize {
    source[..floor_char_boundary(source, byte_offset)]
        .chars()
        .count()
}

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

// ── Format functions (ariadne) ────────────────────────────────────────────

/// Locate the byte offset in `source` corresponding to 1-indexed
/// (line, col).  `col` counts characters within the line, so the in-line
/// advance steps over `col - 1` characters to land on a char boundary.
fn line_col_to_byte(source: &str, line: usize, col: usize) -> usize {
    let mut byte_offset = 0usize;
    for (i, ln) in source.split_inclusive('\n').enumerate() {
        if i + 1 == line {
            let in_line = ln
                .char_indices()
                .nth(col.saturating_sub(1))
                .map_or(ln.len(), |(b, _)| b);
            return byte_offset + in_line;
        }
        byte_offset += ln.len();
    }
    byte_offset
}

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
/// Used by the type-error path when the error lacks a span.
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

/// Short phrase placed next to the primary label, describing the immediate
/// nature of the mismatch.
///
/// The kind's full message goes on the Report;
/// the label is the bite-size pointer that fits next to the underline.
///
/// The label is symmetric in `expected`/`actual` — see the note on
/// `TypeErrorKind::render_message` for why.  Variables get the same
/// Greek letters as the surrounding message (shared `FmtCtx`) so a
/// reader who sees `α` in the message can find `α` in the label.
pub fn label_message_for_kind(kind: &crate::typecheck::TypeErrorKind) -> String {
    use crate::typecheck::{FmtCtx, TypeErrorKind as K, fmt_ty_ctx};
    match kind {
        K::RecursiveRow => "the type loops back into itself here".into(),
        K::TypeTooDeep => "the type nests too deeply here".into(),
        K::TyMismatch { expected, actual } => {
            // Match the orientation of the full message
            // ("couldn't match type X with type Y") so the underline
            // label and the headline read in the same direction.
            let ctx = FmtCtx::for_value_types(&[expected, actual]);
            format!(
                "{} doesn't match {}",
                fmt_ty_ctx(expected, &ctx),
                fmt_ty_ctx(actual, &ctx)
            )
        }
        K::CompTyMismatch { .. } => "types disagree here".into(),
        K::CommandNotCallable { ty, .. } => {
            let ctx = FmtCtx::for_value_types(&[ty]);
            format!("{} cannot be invoked as a command", fmt_ty_ctx(ty, &ctx))
        }
        K::ModeMismatch { .. } => "pipeline channels disagree here".into(),
        K::RowExtraField { label } => format!("no field '{label}' in this record"),
        K::RowMissingField { label } => format!("this record needs field '{label}'"),
        K::CaseNotExhaustive { missing, extra } => match (missing.as_slice(), extra.as_slice()) {
            ([only], []) => format!("no handler for {only}"),
            (some, []) => format!("no handler for {}", some.join(", ")),
            ([], [only]) => format!("handler for {only} that the value never produces"),
            ([], some) => format!(
                "handlers for {} that the value never produces",
                some.join(", ")
            ),
            _ => "case alternatives don't match the value".into(),
        },
        K::CaseLabelTypeMismatch { label, .. } => {
            format!("the handler at {label} is the wrong shape")
        }
        K::CaseOnNonVariant { .. }
        | K::ControlOperatorAsValue { .. }
        | K::HandlerNotFirstClass { .. }
        | K::BuiltinNotFirstClass { .. }
        | K::CannotRedefineBuiltin { .. }
        | K::HandlerShadowedByBinding { .. }
        | K::BuiltinArity { .. }
        | K::FailStatusZero
        | K::MalformedAlias { .. }
        | K::MalformedUnalias { .. }
        | K::IndexIntoThunk
        | K::FieldOnNonRecord { .. }
        | K::DynamicIndexOnScalar { .. } => "here".into(),
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
            label: label_message_for_kind(&err.kind),
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
    ($tag:expr, $($arg:tt)*) => {};
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
