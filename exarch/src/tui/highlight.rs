//! Token-level syntax highlighting for ral code shown in the TUI.
//!
//! The scripts the model writes — coalesced source on a ral block (L3) and
//! the body of an expanded single tool call — read as code rather than flat
//! ink: keyword, string, variable, tag, and punctuation each take a muted
//! hue from the [`super::line`] code palette; everything else keeps the
//! default code ink (white).  The language's own lexer
//! ([`ral_core::syntax::lexer`]) is the definition of what a token *is*, so
//! the colouring tracks the grammar exactly and needs no second parser.
//!
//! The model's scripts are often mid-stream or broken, so highlighting
//! degrades gracefully: a lex error (including an incomplete one — an
//! unterminated string, an unbalanced brace) falls back to one default-ink
//! line per source line.  Either way the rendered text is byte-for-byte the
//! input — the highlighter only *colours* spans, it never adds or drops a
//! character.
//!
//! Nested interpolation streams — a double-quoted string's `$…` splices and
//! `$[…]` / `$name[k]` derefs — are coloured as their single enclosing
//! class, not recursed into.

use super::palette::{CODE_KEYWORD, CODE_STRING, CODE_TAG, CODE_VARIABLE, SLATE};
use ral_core::syntax::is_keyword;
use ral_core::syntax::lexer::{Token, lex};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Highlight `src` as a list of styled lines, one per source line.
///
/// On a clean lex, walks the token stream by byte offset: the gap before
/// each token (indentation, inter-token whitespace, comments — none of which
/// the lexer emits as tokens) renders as default ink, and the token's own
/// text takes its [`class`] style.  On a lex error the whole source falls
/// back to default ink.  Both paths split the styled span stream into lines
/// the same way, so a multi-line string or a blank line round-trips.
pub(super) fn highlight_ral(src: &str) -> Vec<Line<'static>> {
    let spans = match lex(src) {
        Ok(tokens) => highlighted_spans(src, &tokens),
        // Incomplete or invalid code (the common mid-stream case): colour
        // nothing rather than guess, but still lay the source out faithfully.
        Err(_) => vec![Span::styled(src.to_string(), default_ink())],
    };
    into_lines(spans)
}

/// The default code ink — white, the colour every non-classified token and
/// the error fallback wear.
fn default_ink() -> Style {
    Style::default().fg(Color::White)
}

/// Walk the token stream, emitting the gap before each token as default ink
/// and the token's text in its [`class`] style.  Token spans are ordered and
/// non-overlapping and the trailing `Eof` sits at the end of the source, so
/// concatenating the gaps and token texts reproduces `src` exactly.
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

/// The style one token wears.  String, variable (`$…`), and tag (`` `… ``)
/// take their own hue; the bracket/brace/operator punctuation reuses
/// [`SLATE`]; a plain word takes the keyword hue when it is a ral keyword
/// ([`is_keyword`]); everything else — non-keyword words, expression blocks,
/// redirects, separators — keeps the default ink.
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

/// Split a flat styled-span stream into lines on embedded `\n`, mirroring
/// [`str::lines`]: a trailing newline does not yield a final empty line, so
/// the line count matches the source's and the renderers emit no stray panel
/// row.  Each newline is the split point and is dropped, so rejoining the
/// lines with `\n` reproduces the span stream's text.
fn into_lines(spans: Vec<Span<'static>>) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    for span in spans {
        let style = span.style;
        let mut segments = span.content.split('\n');
        if let Some(first) = segments.next()
            && !first.is_empty()
        {
            cur.push(Span::styled(first.to_string(), style));
        }
        for segment in segments {
            lines.push(Line::from(std::mem::take(&mut cur)));
            if !segment.is_empty() {
                cur.push(Span::styled(segment.to_string(), style));
            }
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(Line::from(cur));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain text of a line — span contents joined, styling dropped.
    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The fg colour of the span covering `needle` in a single-line highlight.
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

    /// A clean statement: the keyword, the string literal, and the bound name
    /// each take their expected ink.
    #[test]
    fn classifies_keyword_string_and_name() {
        let src = "let x = 'hi'";
        assert_eq!(fg_of(src, "let"), Some(CODE_KEYWORD));
        assert_eq!(fg_of(src, "'hi'"), Some(CODE_STRING));
        // `x` is a bound name, not a keyword, so it keeps the default ink.
        assert_eq!(fg_of(src, "x"), Some(Color::White));
    }

    /// A deref and a tag take the variable and tag inks; brackets are
    /// punctuation.
    #[test]
    fn classifies_deref_tag_and_punctuation() {
        let src = "[`ok $x]";
        assert_eq!(fg_of(src, "["), Some(SLATE));
        assert_eq!(fg_of(src, "`ok"), Some(CODE_TAG));
        assert_eq!(fg_of(src, "$x"), Some(CODE_VARIABLE));
        assert_eq!(fg_of(src, "]"), Some(SLATE));
    }

    /// Invalid / incomplete code (here an unterminated string) must never
    /// panic: it falls back to a single default-ink span carrying the source.
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

    /// Reassembling the highlighted lines reproduces the input verbatim — the
    /// highlighter only colours, it never drops or adds a character.
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
