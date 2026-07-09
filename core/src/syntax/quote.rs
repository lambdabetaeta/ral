//! Source-level quoting: turn an arbitrary string into ral source that
//! re-lexes back to the same string as a single token.
//!
//! Ral's single-quoted strings have no escape syntax — a `'` in the body
//! cannot be encoded as `\'` (POSIX-style) or by closing-and-reopening
//! (`'foo'\''bar'`).  Instead the lexer accepts a _hash-bumped_ form:
//! `n` `#`s followed by `'` opens a string that closes only on `'`
//! followed by `n` `#`s.  These helpers pick the smallest hash level
//! that does not collide with the body, so the result is always the
//! shortest correct quoting.
//!
//! These are _ral source_ rules, not POSIX shell rules.  The
//! `shell-quote` builtin in `core/src/builtins/strings.rs` keeps its
//! POSIX semantics so it can round-trip with `shell-split`.

use std::borrow::Cow;

use crate::syntax::ast::Word;
use crate::syntax::lexer::{Token, lex};

/// True when `s` can appear in ral source as one bare token whose lexed
/// value is exactly `s`.
///
/// Bareness is a *positional* property of the lexer, not a per-character
/// one: a `#` opens a comment only at token start, `...` is a spread, a
/// `:` splits when whitespace or `]` follows it, `?`/`&` are their own
/// tokens.  A per-char `is_bare_char` scan cannot see any of that and
/// mis-classifies `#foo`, `...`, `foo:` as bare.  Rather than re-derive
/// (and drift from) those rules, the predicate is grounded directly in
/// the lexer: `s` is bare exactly when lexing it yields one
/// `Word::Plain` / `Word::Slash` token whose body is `s`.
///
/// `,` is the one carve-out the lexer can't decide context-free: it
/// stays inside a word at the top level but punctuates inside `[…]`.
/// A completion result may be inserted in either position, so a comma
/// forces quoting regardless of where the standalone lex landed.
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

/// Return ral source for `s` such that lexing the source yields one
/// token whose payload equals `s`.  The strategy in order:
///
/// 1. Bare word if [`is_bare_word`].
/// 2. Plain single-quoted `'…'` if `s` contains no `'`.
/// 3. Hash-bumped single-quoted `#'…'#` (or with more `#`s) at the
///    smallest level whose closing run `'######` does not occur inside
///    `s`.
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

/// `Cow`-returning variant: bare words pass through borrowed, so the
/// hot path in completion never allocates.
pub fn quote_word_if_needed(s: &str) -> Cow<'_, str> {
    if is_bare_word(s) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(quote_word(s))
    }
}

/// Smallest `n ≥ 1` such that the closing sequence `'` + `n` `#`s does
/// not appear inside `s`.  Picking the level by direct search keeps the
/// quoting minimal without resorting to escape trickery.
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

    /// Re-lex `src` and assert it yields exactly one token whose
    /// payload (string body or bare word) equals `expected`.
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

    // ── quote_word: choose minimal form ─────────────────────────────

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
        // Bare `~foo` would tilde-expand; quoting prevents that.
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
        // Body contains `'#`, so #'…'# would mis-close.  Level 2 (##'…'##) suffices.
        assert_eq!(quote_word("a'#b"), "##'a'#b'##");
    }

    #[test]
    fn quote_word_empty_is_empty_quotes() {
        assert_eq!(quote_word(""), "''");
    }

    #[test]
    fn quote_word_unicode_is_bare() {
        // Non-ASCII letters are not metacharacters, so they stay bare.
        assert_eq!(quote_word("日本語"), "日本語");
    }

    #[test]
    fn quote_word_unicode_with_space_quotes() {
        assert_eq!(quote_word("a 日 b"), "'a 日 b'");
    }

    // ── quote_word_if_needed borrows bare words ─────────────────────

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

    // ── round-trips through the actual lexer ─────────────────────────

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
            // F10: positional metacharacters that a per-char scan missed.
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

    /// F10: the positional metacharacter shapes are *not* bare words —
    /// quoting them is mandatory for a faithful round-trip.
    #[test]
    fn bare_word_rejects_positional_metacharacters() {
        for s in ["#foo", "#f.txt#", "foo:", "...", ":", "?x", "&x"] {
            assert!(!is_bare_word(s), "{s:?} must not be treated as bare");
        }
    }

    /// An embedded `:` not followed by whitespace stays in one word, so
    /// `host:5432` is still bare (the lexer does not split there).
    #[test]
    fn bare_word_keeps_embedded_colon() {
        assert!(is_bare_word("host:5432"));
    }
}
