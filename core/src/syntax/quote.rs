//! Turn a string into ral source that re-lexes back to that same string
//! as a single token.
//!
//! Ral's single-quoted strings have no escapes, so a body containing `'`
//! is written hash-bumped: `n` `#`s then `'`, closing only on `'` then
//! `n` `#`s.  These are ral's rules, not POSIX's — the `shell-quote`
//! builtin in `core/src/builtins/strings.rs` keeps POSIX semantics so it
//! round-trips with `shell-split`.

use std::borrow::Cow;

use crate::syntax::ast::Word;
use crate::syntax::lexer::{Token, lex};

/// True when `s` lexes as one bare word whose value is exactly `s`.
///
/// Bareness is positional — `#` opens a comment only at token start, `:`
/// splits only before whitespace or `]` — so it is settled by lexing
/// rather than by a per-character scan like the lexer's `is_bare_char`,
/// which would call `#foo`, `...` and `foo:` bare.  `,` is the case
/// lexing cannot settle: it stays inside a word at top level and
/// punctuates inside `[…]`, so it always forces quoting.
pub fn is_bare_word(s: &str) -> bool {
    if s.is_empty() || s.contains(',') {
        return false;
    }
    let Ok(tokens) = lex(s) else {
        return false;
    };
    let mut significant = tokens.iter().filter(|(t, _)| *t != Token::Eof);
    matches!(
        (significant.next(), significant.next()),
        (Some((Token::Word(Word::Plain(w) | Word::Slash(w)), _)), None) if w == s
    )
}

/// Ral source for `s`: bare where it can be, else single-quoted, else
/// hash-bumped to the smallest level the body leaves free.
pub fn quote_word(s: &str) -> String {
    if is_bare_word(s) {
        return s.to_string();
    }
    if !s.contains('\'') {
        return format!("'{s}'");
    }
    let level = min_bump_level(s);
    let hashes = "#".repeat(level);
    format!("{hashes}'{s}'{hashes}")
}

/// [`quote_word`] returning a `Cow`, so bare words pass through borrowed.
pub fn quote_word_if_needed(s: &str) -> Cow<'_, str> {
    if is_bare_word(s) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(quote_word(s))
    }
}

/// Smallest `n ≥ 1` whose closer, `'` followed by `n` `#`s, is absent
/// from `s`.
fn min_bump_level(s: &str) -> usize {
    let mut level = 1;
    loop {
        let closer = format!("'{}", "#".repeat(level));
        if !s.contains(&closer) {
            return level;
        }
        level += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::lexer::{Token, lex};

    fn lex_round_trip(src: &str, expected: &str) {
        let toks = lex(src).expect("lex");
        let payloads: Vec<&Token> = toks
            .iter()
            .map(|(t, _)| t)
            .filter(|t| **t != Token::Eof)
            .collect();
        assert_eq!(
            payloads.len(),
            1,
            "expected one non-Eof token, got {payloads:?} from {src:?}"
        );
        let body = match payloads[0] {
            Token::SingleQuoted(s)
            | Token::Word(
                crate::syntax::ast::Word::Plain(s) | crate::syntax::ast::Word::Slash(s),
            ) => s.clone(),
            other => panic!("unexpected token kind for {src:?}: {other:?}"),
        };
        assert_eq!(body, expected, "round-trip failed for source {src:?}");
    }

    // ── is_bare_word ─────────────────────────────────────────────────

    #[test]
    fn bare_word_accepts_plain_identifier() {
        assert!(is_bare_word("hello"));
        assert!(is_bare_word("hello.txt"));
        assert!(is_bare_word("path/to/file"));
        assert!(is_bare_word("a-b_c"));
    }

    #[test]
    fn bare_word_rejects_empty() {
        assert!(!is_bare_word(""));
    }

    #[test]
    fn bare_word_rejects_tilde_leading() {
        assert!(!is_bare_word("~/foo"));
        assert!(!is_bare_word("~"));
    }

    #[test]
    fn bare_word_rejects_metacharacters() {
        for s in [
            "a b", // space
            "a|b", "a$b", "a!b", "a~b", "a<b", "a>b", "a\"b", "a'b", "a`b", "a(b", "a)b", "a;b",
            "a^b", "a[b", "a]b", "a{b", "a}b", "a,b", // comma (context-sensitive)
        ] {
            assert!(!is_bare_word(s), "{s:?} should not be bare");
        }
    }

    // ── quote_word ───────────────────────────────────────────────────

    #[test]
    fn quote_word_bare_passes_through() {
        assert_eq!(quote_word("hello.txt"), "hello.txt");
    }

    #[test]
    fn quote_word_space_uses_single_quotes() {
        assert_eq!(quote_word("a b.txt"), "'a b.txt'");
    }

    #[test]
    fn quote_word_dollar_uses_single_quotes() {
        assert_eq!(quote_word("$HOME"), "'$HOME'");
    }

    #[test]
    fn quote_word_bang_uses_single_quotes() {
        assert_eq!(quote_word("a!b"), "'a!b'");
    }

    #[test]
    fn quote_word_tilde_uses_single_quotes() {
        // Bare `~foo` would tilde-expand.
        assert_eq!(quote_word("~foo"), "'~foo'");
    }

    #[test]
    fn quote_word_brackets_use_single_quotes() {
        assert_eq!(quote_word("[a]"), "'[a]'");
        assert_eq!(quote_word("{a}"), "'{a}'");
        assert_eq!(quote_word("(a)"), "'(a)'");
    }

    #[test]
    fn quote_word_embedded_quote_bumps_hash() {
        assert_eq!(quote_word("it's.txt"), "#'it's.txt'#");
    }

    #[test]
    fn quote_word_close_run_in_body_bumps_further() {
        // The body holds `'#`, which would close a level-1 quote early.
        assert_eq!(quote_word("a'#b"), "##'a'#b'##");
    }

    #[test]
    fn quote_word_empty_is_empty_quotes() {
        assert_eq!(quote_word(""), "''");
    }

    #[test]
    fn quote_word_unicode_is_bare() {
        // Non-ASCII letters are not metacharacters.
        assert_eq!(quote_word("日本語"), "日本語");
    }

    #[test]
    fn quote_word_unicode_with_space_quotes() {
        assert_eq!(quote_word("a 日 b"), "'a 日 b'");
    }

    // ── quote_word_if_needed ─────────────────────────────────────────

    #[test]
    fn quote_word_if_needed_borrows_bare() {
        let s = "hello.txt";
        let cow = quote_word_if_needed(s);
        assert!(matches!(cow, Cow::Borrowed(_)));
        assert_eq!(cow.as_ref(), "hello.txt");
    }

    #[test]
    fn quote_word_if_needed_owned_on_quoting() {
        let cow = quote_word_if_needed("a b");
        assert!(matches!(cow, Cow::Owned(_)));
        assert_eq!(cow.as_ref(), "'a b'");
    }

    // ── lexer round-trips ────────────────────────────────────────────

    #[test]
    fn lex_round_trips_for_assorted_inputs() {
        for input in [
            "hello",
            "hello.txt",
            "path/to/file",
            "a b.txt",
            "$HOME",
            "a!b",
            "~foo",
            "it's.txt",
            "a'#b",
            "日本語",
            "",
            // Positional metacharacters, invisible to a per-char scan.
            "#foo",    // leading `#` opens a comment
            "#f.txt#", // emacs backup name
            "foo:",    // trailing `:` splits off a Colon
            "...",     // a spread token
            ":",       // a bare Colon
            "?x",      // `?` is its own token
            "&x",      // `&` is its own token
        ] {
            let src = quote_word(input);
            lex_round_trip(&src, input);
        }
    }

    #[test]
    fn bare_word_rejects_positional_metacharacters() {
        for s in ["#foo", "#f.txt#", "foo:", "...", ":", "?x", "&x"] {
            assert!(!is_bare_word(s), "{s:?} must not be treated as bare");
        }
    }

    #[test]
    fn bare_word_keeps_embedded_colon() {
        assert!(is_bare_word("host:5432"));
    }
}
