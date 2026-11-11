//! Token classification shared by every syntax-highlighting front end.
//!
//! Exarch's TUI and synod's window both colour ral source by asking the
//! language's own lexer what each token is, never a second parser of their
//! own.

use super::is_keyword;
use crate::syntax::lexer::{Lexeme, Token, lex};
use std::ops::Range;

/// The classes a highlighter distinguishes.
///
/// Not one variant per [`Token`] case: several tokens (the braces, brackets,
/// parens, comma, pipe, colon, spread, caret, question, bang, operators) share
/// [`Class::Punct`], and every token with no hue of its own — an identifier,
/// `Newline`, `Semi`, a redirect, `$[…]` — is [`Class::Plain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Keyword,
    String,
    Tag,
    Variable,
    Punct,
    Plain,
}

/// Classify `src` token by token, in source order.
///
/// Total over every non-zero-width token — only the trailing `Eof` is
/// skipped — so gaps between returned ranges are exactly the whitespace and
/// comments the lexer never emits a token for.  A lex error — the common
/// mid-stream case, an unterminated string or an unbalanced brace while the
/// user is still typing — yields an empty vec rather than a guess.
pub fn classify(src: &str) -> Vec<(Range<usize>, Class)> {
    let Ok(tokens) = lex(src) else {
        return Vec::new();
    };
    tokens
        .iter()
        .filter_map(|Lexeme { token, span }| {
            let range = span.range();
            (!range.is_empty()).then(|| (range, class(token)))
        })
        .collect()
}

/// The class one token wears.
fn class(tok: &Token) -> Class {
    match tok {
        Token::SingleQuoted(_) | Token::DoubleQuoted(_) => Class::String,
        Token::Variable { .. } => Class::Variable,
        Token::Tag(_) => Class::Tag,
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
        | Token::Bang
        | Token::Op(_) => Class::Punct,
        _ => match tok.as_plain_word() {
            Some(word) if is_keyword(word) => Class::Keyword,
            _ => Class::Plain,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whitespace and `#` comments: all the lexer leaves between tokens.
    fn blank_or_comment(gap: &str) -> bool {
        gap.lines().all(|line| {
            let line = line.trim_start();
            line.is_empty() || line.starts_with('#')
        })
    }

    #[test]
    fn ranges_are_ordered_in_bounds_on_char_boundaries_and_gapped_only_by_blanks() {
        for src in [
            "echo 'héllo 日本' \"😀 $x é\"",
            "let x = \"a !{echo é} b $y[k] $[1 + 2] c\"",
            "echo a # trailing é comment\necho b",
            "  # lead\n echo a  \n",
            "[`ok: 1, `err: { |e| return $e }] | cat > out",
            "if $c { a } else { b }",
        ] {
            let classes = classify(src);
            assert!(!classes.is_empty(), "{src:?} lexes");
            let mut end = 0;
            for (range, _) in &classes {
                let gap = src
                    .get(end..range.start)
                    .unwrap_or_else(|| panic!("{src:?}: {end}..{} is no slice", range.start));
                assert!(blank_or_comment(gap), "{src:?}: gap {gap:?}");
                assert!(
                    src.get(range.clone()).is_some_and(|t| !t.is_empty()),
                    "{src:?}: {range:?} is no slice"
                );
                end = range.end;
            }
            assert!(blank_or_comment(&src[end..]), "{src:?}: tail");
        }
    }

    #[test]
    fn lex_error_yields_empty() {
        assert_eq!(classify("let x = 'unterminated"), Vec::new());
    }
}
