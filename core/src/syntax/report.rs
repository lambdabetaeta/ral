//! A parse failure as a [`Report`].

use super::lexer::LexErrorKind;
use super::parser::{ParseError, ParseErrorKind};
use super::quote::one_word;
use crate::diagnostic::{Label, Report};
use crate::source::{Source, Span};

fn label(span: Option<Span>, text: impl Into<String>) -> Label {
    Label {
        span,
        text: text.into(),
    }
}

impl ParseError {
    /// Two labels where the failure's structure gives two; otherwise one red
    /// label on the offending token, or on the end of input.
    pub fn report(&self, src: &Source) -> Report {
        match &self.kind {
            ParseErrorKind::Lex(kind) => lex_report(src, kind),
            ParseErrorKind::Touching { first, second, run } => {
                Some(self.touching(src, *first, *second, *run))
            }
            ParseErrorKind::Plain => None,
        }
        .unwrap_or_else(|| Report {
            code: Some("P0001"),
            message: self.message.clone(),
            at: Some(label(self.span, "here")),
            also: None,
            hint: None,
        })
    }

    /// Two touching atoms, with both readings spelled out in the help.
    fn touching(&self, src: &Source, first: Span, second: Span, run: Span) -> Report {
        let text = |span: Span| &src.as_str()[span.range()];
        let two = format!("two arguments: `{} {}`", text(first), text(second));
        Report {
            code: Some("P0002"),
            message: self.message.clone(),
            at: Some(label(
                Some(second),
                "this word starts with no space before it",
            )),
            also: Some(label(Some(first), "this word ends here")),
            hint: Some(match one_word(text(run)) {
                Some(one) => format!("{two}; one argument: {one}"),
                None => two,
            }),
        }
    }
}

/// Prose for the help line: "`{…}` opened at 1:6, which itself contains …".
/// Line/column is recovered from `src` here, not carried on the error.
fn describe_inner(src: &Source, kind: &LexErrorKind) -> String {
    let pos = |span: Span| {
        let (line, col) = src.line_col(span.start);
        format!("{line}:{col}")
    };
    match kind {
        LexErrorKind::UnterminatedBalanced {
            open,
            close,
            opened,
        } => format!("`{open}…{close}` opened at {}", pos(*opened)),
        LexErrorKind::UnclosedDeref { opened } => format!("`$(…)` opened at {}", pos(*opened)),
        LexErrorKind::UnterminatedString {
            form,
            opened,
            inner,
        } => {
            let head = format!("{form} opened at {}", pos(*opened));
            match inner {
                Some(i) => format!("{head}, which itself contains {}", describe_inner(src, i)),
                None => head,
            }
        }
        LexErrorKind::Other(_) => "an unrelated lexer error".into(),
    }
}

/// `None` for `Other(_)`, so the caller falls back to the single-label form.
/// Codes are never reused: the next lex diagnostic takes L0006.
fn lex_report(src: &Source, kind: &LexErrorKind) -> Option<Report> {
    let report = |code, opened: &Span, text, also, hint| Report {
        code: Some(code),
        message: kind.headline(),
        at: Some(label(Some(*opened), text)),
        also,
        hint,
    };
    Some(match kind {
        LexErrorKind::UnterminatedString {
            form,
            opened,
            inner,
        } => report(
            "L0001",
            opened,
            format!("{form} opened here"),
            Some(label(
                None,
                format!("expected closing `{}` here", form.closing()),
            )),
            inner.as_ref().map(|i| {
                format!(
                    "nested {} was not closed before EOF",
                    describe_inner(src, i)
                )
            }),
        ),
        LexErrorKind::UnterminatedBalanced {
            open,
            close,
            opened,
        } => report(
            "L0002",
            opened,
            format!("`{open}` opened here"),
            None,
            Some(format!("expected closing `{close}` before end of input")),
        ),
        LexErrorKind::UnclosedDeref { opened } => report(
            "L0003",
            opened,
            "`$(` opened here".into(),
            None,
            Some("expected closing `)` before end of input".into()),
        ),
        LexErrorKind::Other(_) => return None,
    })
}
