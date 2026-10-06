//! The report algebra and its one drawer.
//!
//! Every user-visible parse, type, and runtime error becomes a [`Report`] and
//! is drawn here: through `ariadne` when a span points somewhere, as a
//! one-liner when it does not.  Each error type builds its own report; none
//! draws.

use crate::ansi::{BOLD_CYAN, BOLD_RED, RESET};
use crate::source::{Source, Span};
use crate::terminal::stderr_color;
use ariadne::{Color, Config, IndexType, ReportKind};
use std::fmt::Write;
use std::ops::Range;

#[cfg(test)]
mod tests;

/// A span and the phrase placed beside its underline; `None` is the end of
/// the input.
#[derive(Debug, Clone)]
pub struct Label {
    pub span: Option<Span>,
    pub text: String,
}

/// One diagnostic: a red primary label, an optional yellow secondary, an
/// optional help line.  `at: None` draws no source at all.
#[derive(Debug, Clone)]
pub struct Report {
    pub code: Option<&'static str>,
    pub message: String,
    pub at: Option<Label>,
    pub also: Option<Label>,
    pub hint: Option<String>,
}

/// A compile failure with the text its carets point into.
///
/// That text is never registered in the session's
/// [`SourceDb`](crate::source::SourceDb): a failed compile leaves no live span
/// behind, and the registry is append-only because live spans index it.
#[derive(Debug, Clone)]
pub struct Rejection {
    pub source: Source,
    pub reports: Vec<Report>,
    /// The status a run that failed so exits on.
    pub status: i32,
}

impl Rejection {
    pub fn render(&self) -> String {
        self.reports
            .iter()
            .map(|r| r.render(&self.source))
            .collect()
    }
}

/// `(error, hint, reset)`; empty strings when colour is off, so one `format!`
/// serves both.
pub(crate) fn palette() -> (&'static str, &'static str, &'static str) {
    if stderr_color() {
        (BOLD_RED, BOLD_CYAN, RESET)
    } else {
        ("", "", "")
    }
}

impl Report {
    /// Drawn into `src`, which `at` and `also` index; with no `at`, the
    /// one-liner.
    pub fn render(&self, src: &Source) -> String {
        let Some(at) = &self.at else {
            return self.plain();
        };
        let name = &**src.name();
        let label = |l: &Label, color| {
            ariadne::Label::new((name, caret(src, l.span)))
                .with_message(&l.text)
                .with_color(color)
        };
        let mut report = ariadne::Report::build(ReportKind::Error, (name, caret(src, at.span)))
            .with_config(
                Config::default()
                    .with_color(stderr_color())
                    .with_index_type(IndexType::Byte),
            )
            .with_message(&self.message)
            .with_label(label(at, Color::Red));
        if let Some(code) = self.code {
            report = report.with_code(code);
        }
        if let Some(also) = &self.also {
            report = report.with_label(label(also, Color::Yellow));
        }
        if let Some(hint) = &self.hint {
            report = report.with_help(hint);
        }
        let mut buf = Vec::new();
        let _ = report
            .finish()
            .write((name, ariadne::Source::from(src.as_str())), &mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// The spanless one-liner.
    pub fn plain(&self) -> String {
        let (head, cyan, reset) = palette();
        let mut out = String::new();
        let _ = match self.code {
            Some(c) => writeln!(out, "{head}[{c}] Error{reset}: {}", self.message),
            None => writeln!(out, "{head}Error{reset}: {}", self.message),
        };
        if let Some(h) = &self.hint {
            let _ = writeln!(out, "  {cyan}help{reset}: {h}");
        }
        out
    }
}

/// The bytes a caret underlines: `span` widened to one whole char where it is
/// empty, the last char at the end of the input.  Ariadne drops a label that
/// is out of range or empty, so every caret is made drawable here.
fn caret(src: &Source, span: Option<Span>) -> Range<usize> {
    let text = src.as_str();
    let (start, end) = span.map_or((text.len(), text.len()), |s| {
        (s.start as usize, s.end as usize)
    });
    let start = text.floor_char_boundary(start);
    let end = text.ceil_char_boundary(end.max(start));
    match (start < end, start < text.len()) {
        (true, _) => start..end,
        (false, true) => start..text.ceil_char_boundary(start + 1),
        (false, false) => text.floor_char_boundary(start.saturating_sub(1))..start,
    }
}
