//! Token classification shared by every syntax-highlighting front end.
//!
//! Exarch's TUI and synod's window both colour ral source by asking the
//! language's own lexer what each token is, never a second parser of their
//! own.

use super::is_keyword;
use crate::syntax::lexer::{Token, lex};
use std::ops::Range;

/// The classes a highlighter distinguishes.
///
/// Not one variant per [`Token`] case: several tokens (the braces, brackets,
/// parens, comma, pipe, colon, spread, caret, question, bang) share
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
        .filter_map(|(tok, span)| {
            let range = span.range();
            (!range.is_empty()).then(|| (range, class(tok)))
        })
        .collect()
}

/// The class one token wears.
fn class(tok: &Token) -> Class {
    match tok {
        Token::SingleQuoted(_) | Token::DoubleQuoted(_) => Class::String,
        Token::Variable(_) => Class::Variable,
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
        | Token::Bang => Class::Punct,
        _ => match tok.as_plain_word() {
            Some(word) if is_keyword(word) => Class::Keyword,
            _ => Class::Plain,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(src: &str, classes: &[(Range<usize>, Class)], needle: &str) -> Class {
        let start = src.find(needle).unwrap();
        let range = start..start + needle.len();
        classes
            .iter()
            .find(|(r, _)| *r == range)
            .unwrap_or_else(|| panic!("no range exactly {needle:?} in {src:?}"))
            .1
    }

    #[test]
    fn classifies_keyword() {
        let src = "let x = 'hi'";
        assert_eq!(find(src, &classify(src), "let"), Class::Keyword);
    }

    #[test]
    fn classifies_string() {
        let src = "let x = 'hi'";
        assert_eq!(find(src, &classify(src), "'hi'"), Class::String);
    }

    #[test]
    fn classifies_tag() {
        let src = "[`ok $x]";
        assert_eq!(find(src, &classify(src), "`ok"), Class::Tag);
    }

    #[test]
    fn classifies_variable() {
        let src = "[`ok $x]";
        assert_eq!(find(src, &classify(src), "$x"), Class::Variable);
    }

    #[test]
    fn classifies_identifier_as_plain() {
        let src = "let x = 'hi'";
        assert_eq!(find(src, &classify(src), "x"), Class::Plain);
    }

    #[test]
    fn classifies_punctuation() {
        let src = "[`ok $x]";
        let classes = classify(src);
        assert_eq!(find(src, &classes, "["), Class::Punct);
        assert_eq!(find(src, &classes, "]"), Class::Punct);
    }

    #[test]
    fn lex_error_yields_empty() {
        assert_eq!(classify("let x = 'unterminated"), Vec::new());
    }
}
