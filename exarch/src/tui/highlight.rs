//! Token-level syntax highlighting for the ral source the TUI shows in its
//! code panels.
//!
//! The language's own lexer (`ral_core::syntax::lexer`) is the definition of
//! what a token is, so the colouring tracks the grammar and needs no second
//! parser.  Whatever happens the rendered text is byte-for-byte the input:
//! nothing here adds or drops a character, it only hands spans a hue from
//! [`super::palette`].

use super::line::fold_styled_lines;
use super::palette::{CODE_KEYWORD, CODE_STRING, CODE_TAG, CODE_VARIABLE, SLATE};
use ral_core::syntax::highlight::{Class, classify};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Highlight `src` as one styled line per source line.
///
/// The lexer emits neither whitespace nor comments, so the gap before each
/// token renders as default ink.  A lex error — the common mid-stream case,
/// an unterminated string or an unbalanced brace — colours nothing at all
/// rather than guess.
pub(super) fn highlight_ral(src: &str) -> Vec<Line<'static>> {
    into_lines(highlight_ral_spans(src))
}

/// [`highlight_ral`] unsplit: the styled run for a source that is to be laid
/// out by a caller of its own, such as a value sharing a row with a label.
pub(super) fn highlight_ral_spans(src: &str) -> Vec<Span<'static>> {
    highlighted_spans(src, &classify(src))
}

fn default_ink() -> Style {
    Style::default().fg(Color::White)
}

/// Emit each classified range in its `style` and the gap before it as
/// default ink.  Ranges are ordered and non-overlapping, and `classify` is
/// total over non-zero-width tokens, so the concatenation is `src` exactly.
fn highlighted_spans(src: &str, classes: &[(std::ops::Range<usize>, Class)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    for (range, class) in classes {
        if range.start > cursor {
            spans.push(Span::styled(
                src[cursor..range.start].to_string(),
                default_ink(),
            ));
        }
        spans.push(Span::styled(src[range.clone()].to_string(), style(*class)));
        cursor = range.end;
    }
    if cursor < src.len() {
        spans.push(Span::styled(src[cursor..].to_string(), default_ink()));
    }
    spans
}

/// The style one [`Class`] wears.
fn style(class: Class) -> Style {
    let fg = match class {
        Class::Keyword => CODE_KEYWORD,
        Class::String => CODE_STRING,
        Class::Tag => CODE_TAG,
        Class::Variable => CODE_VARIABLE,
        Class::Punct => SLATE,
        Class::Plain => return default_ink(),
    };
    Style::default().fg(fg)
}

/// Split the span stream on embedded `\n`, mirroring [`str::lines`]: a
/// trailing newline yields no final empty line, so no panel gains a stray row.
fn into_lines(spans: Vec<Span<'static>>) -> Vec<Line<'static>> {
    fold_styled_lines(
        spans.into_iter().map(|s| (s.content.into_owned(), s.style)),
        false,
    )
}

#[cfg(test)]
mod tests;
