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
use ral_core::syntax::is_keyword;
use ral_core::syntax::lexer::{Token, lex};
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
    let spans = match lex(src) {
        Ok(tokens) => highlighted_spans(src, &tokens),
        Err(_) => vec![Span::styled(src.to_string(), default_ink())],
    };
    into_lines(spans)
}

fn default_ink() -> Style {
    Style::default().fg(Color::White)
}

/// Emit each token's text in its `class` style and the gap before it as
/// default ink.  Token spans are ordered and non-overlapping and the trailing
/// `Eof` sits at the end of the source, so the concatenation is `src` exactly.
fn highlighted_spans(src: &str, tokens: &[(Token, ral_core::source::Span)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    for (tok, span) in tokens {
        let range = span.range();
        if range.start > cursor {
            spans.push(Span::styled(
                src[cursor..range.start].to_string(),
                default_ink(),
            ));
        }
        if range.end > range.start {
            spans.push(Span::styled(src[range.clone()].to_string(), class(tok)));
        }
        cursor = range.end;
    }
    if cursor < src.len() {
        spans.push(Span::styled(src[cursor..].to_string(), default_ink()));
    }
    spans
}

/// The style one token wears.  Punctuation shares [`SLATE`], a plain word
/// takes the keyword hue only when [`is_keyword`] says so, and a string or
/// deref is coloured whole — its nested `$…` splices are not recursed into.
fn class(tok: &Token) -> Style {
    let fg = match tok {
        Token::SingleQuoted(_) | Token::DoubleQuoted(_) => CODE_STRING,
        Token::Deref(_) | Token::Dollar => CODE_VARIABLE,
        Token::Tag(_) => CODE_TAG,
        Token::LBrace
        | Token::RBrace
        | Token::LBracket
        | Token::RBracket
        | Token::LParen
        | Token::RParen
        | Token::Comma
        | Token::Pipe
        | Token::Colon
        | Token::Spread
        | Token::Caret
        | Token::Question
        | Token::Bang => SLATE,
        _ => match tok.as_plain_word() {
            Some(word) if is_keyword(word) => CODE_KEYWORD,
            _ => Color::White,
        },
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
mod tests {
    use super::*;

    /// A line's text, styling dropped.
    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The fg of the span whose text is exactly `needle`, in a one-line source.
    fn fg_of(src: &str, needle: &str) -> Option<Color> {
        let lines = highlight_ral(src);
        assert_eq!(lines.len(), 1, "expected one line for {src:?}");
        lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == needle)
            .unwrap_or_else(|| panic!("no span exactly {needle:?} in {src:?}"))
            .style
            .fg
    }

    #[test]
    fn classifies_keyword_string_and_name() {
        let src = "let x = 'hi'";
        assert_eq!(fg_of(src, "let"), Some(CODE_KEYWORD));
        assert_eq!(fg_of(src, "'hi'"), Some(CODE_STRING));
        // `x` is a bound name, not a keyword.
        assert_eq!(fg_of(src, "x"), Some(Color::White));
    }

    #[test]
    fn classifies_deref_tag_and_punctuation() {
        let src = "[`ok $x]";
        assert_eq!(fg_of(src, "["), Some(SLATE));
        assert_eq!(fg_of(src, "`ok"), Some(CODE_TAG));
        assert_eq!(fg_of(src, "$x"), Some(CODE_VARIABLE));
        assert_eq!(fg_of(src, "]"), Some(SLATE));
    }

    #[test]
    fn invalid_code_falls_back_to_default_ink() {
        let lines = highlight_ral("let x = 'unterminated");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|s| s.style.fg == Some(Color::White)),
            "fallback colours nothing"
        );
        assert_eq!(text(&lines[0]), "let x = 'unterminated");
    }

    #[test]
    fn reassembled_text_equals_input() {
        for src in [
            "let x = 'hi'",
            "ls | filter { |f| $f }",
            "map [1, 2, 3] { |n| $n }\nlet y = `tag",
            "echo a # trailing comment",
        ] {
            let joined: Vec<String> = highlight_ral(src).iter().map(text).collect();
            assert_eq!(joined.join("\n"), src, "round-trip differs for {src:?}");
        }
    }
}
