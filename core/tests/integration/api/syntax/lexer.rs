//! Progress, original-source spans, and strict/editor agreement.

use ral_core::ir::WriteMode;
use ral_core::source::{FileId, Span};
use ral_core::syntax::lexer::{
    LexErrorKind, Lexeme, RedirectOp, StringForm, StringPart, Token, lex, lex_open,
};
use ral_core::syntax::parser::{needs_continuation, parse};
use std::ops::Range;

fn checked_span(source: &str, span: Span, parent: &Range<usize>) -> Range<usize> {
    let range = span.range();
    assert!(
        parent.start <= range.start && range.end <= parent.end,
        "{source:?}: {range:?}"
    );
    assert!(source.get(range.clone()).is_some(), "{source:?}: {range:?}");
    assert_eq!(span, Span::new(FileId::DUMMY, span.start, span.end));
    range
}

fn check_stream(source: &str, tokens: &[Lexeme], parent: Range<usize>) {
    let mut end = parent.start;
    for Lexeme { token, span } in tokens {
        let range = checked_span(source, *span, &parent);
        assert!(end <= range.start, "{source:?}: overlapping tokens");
        assert!(
            token == &Token::Eof || !range.is_empty(),
            "{source:?}: no progress"
        );
        end = range.end;
        match token {
            Token::Expr(body) => check_stream(source, body, range),
            Token::DoubleQuoted(parts) => {
                let mut end = range.start;
                for part in parts {
                    let part_range = checked_span(source, part.span.expect("located part"), &range);
                    assert!(
                        end <= part_range.start && !part_range.is_empty(),
                        "{source:?}"
                    );
                    end = part_range.end;
                    if let StringPart::Splice(body) = &part.item {
                        check_stream(source, body, part_range);
                    }
                }
            }
            _ => {}
        }
    }
}

fn check_contract(source: &str) {
    let editor = lex_open(source);
    if let Some(editor) = &editor {
        check_stream(source, &editor.tokens, 0..source.len());
        let eof = editor.tokens.last().expect("EOF");
        assert_eq!(eof.token, Token::Eof);
        assert_eq!(eof.span.range(), source.len()..source.len());
        assert_eq!(
            editor
                .tokens
                .iter()
                .filter(|l| l.token == Token::Eof)
                .count(),
            1
        );
        if let Some(span) = editor.unclosed_string {
            checked_span(source, span, &(0..source.len()));
            assert_eq!(span.end as usize, source.len());
        }
    }
    match lex(source) {
        Ok(tokens) => {
            let editor = editor.expect("complete input must lex in the editor");
            assert_eq!(editor.tokens, tokens, "{source:?}");
            assert_eq!(editor.unclosed_string, None, "{source:?}");
        }
        Err(error) => match error.kind {
            LexErrorKind::UnterminatedString { .. }
            | LexErrorKind::UnterminatedBalanced { .. }
            | LexErrorKind::UnclosedDeref { .. } => {}
            _ => assert!(
                editor.is_none(),
                "editor concealed {error:?} for {source:?}"
            ),
        },
    }
}

#[test]
fn an_open_group_followed_by_a_comment_reaches_eof() {
    for (source, opener) in [
        ("{ # comment", Token::LBrace),
        ("[ # comment", Token::LBracket),
    ] {
        let editor = lex_open(source).unwrap();
        assert_eq!(
            editor.tokens.iter().map(|l| &l.token).collect::<Vec<_>>(),
            [&opener, &Token::Eof]
        );
        check_contract(source);
    }
    for source in ["$[1 # comment", "\"!{echo # comment", "\"$x[k # comment"] {
        check_contract(source);
        assert!(lex_open(source).is_some(), "{source:?}");
    }
}

#[test]
fn editor_delimiters_are_never_synthesized() {
    let source = format!("{} # tail", "{[".repeat(4096));
    let editor = lex_open(&source).unwrap();
    assert_eq!(editor.tokens.len(), 8193);
    assert!(
        editor.tokens[..8192]
            .iter()
            .all(|l| matches!(l.token, Token::LBrace | Token::LBracket))
    );
    assert_eq!(editor.unclosed_string, None);
    check_stream(&source, &editor.tokens, 0..source.len());
}

#[test]
fn unfinished_strings_keep_their_written_extent() {
    for (source, form, opener_len) in [
        ("\"", StringForm::DoubleQuoted, 1),
        ("\"猫", StringForm::DoubleQuoted, 1),
        ("\"a\\\"", StringForm::DoubleQuoted, 1),
        ("\"a\\", StringForm::DoubleQuoted, 1),
        ("'", StringForm::SingleQuoted(0), 1),
        ("#'猫", StringForm::SingleQuoted(1), 2),
        ("##'abc'#", StringForm::SingleQuoted(2), 3),
    ] {
        let error = lex(source).unwrap_err();
        let LexErrorKind::UnterminatedString {
            form: actual,
            opened,
            inner: None,
        } = error.kind
        else {
            panic!("{source:?}: {error:?}");
        };
        assert_eq!(actual, form);
        assert_eq!(opened.range(), 0..opener_len);
        let editor = lex_open(source).unwrap();
        assert_eq!(editor.unclosed_string.unwrap().range(), 0..source.len());
        assert_eq!(editor.tokens[0].span.range(), 0..source.len());
        check_contract(source);
    }
}

#[test]
fn unfinished_metadata_names_the_outermost_string() {
    for (source, start) in [
        ("echo \"!{echo \"猫", 5),
        ("é \"!{echo \"猫", 3),
        ("$[\"猫", 2),
        ("\"closed\" $[\"猫", 11),
    ] {
        let editor = lex_open(source).unwrap();
        assert_eq!(
            editor.unclosed_string.unwrap().range(),
            start..source.len(),
            "{source:?}"
        );
        check_contract(source);
    }
}

#[test]
fn editor_recovery_obeys_the_recursive_nesting_limit() {
    ral_core::compile::on_compile_stack(|| {
        for depth in [64, 65] {
            let open = "$[".repeat(depth);
            let closed = format!("{open}0{}", "]".repeat(depth));
            assert_eq!(lex_open(&open).is_some(), depth == 64);
            assert_eq!(lex_open(&closed).is_some(), depth == 64);
            assert_eq!(lex(&closed).is_ok(), depth == 64);
        }
    });
}

#[test]
fn a_trailing_backslash_asks_for_the_next_line() {
    for (prefix, suffix) in [
        ("echo \"foo\\", "\nbar\""),
        ("echo \"!{echo \"猫\\", "\nbar\"}\""),
    ] {
        assert!(needs_continuation(prefix), "{prefix:?}");
        assert!(parse(&format!("{prefix}{suffix}")).is_ok());
        check_contract(prefix);
    }
    for source in ["\"\\q\"", "\"\\xZ\"", "\"\\u{110000}\""] {
        assert!(!needs_continuation(source), "{source:?}");
        assert!(lex_open(source).is_none(), "{source:?}");
    }
}

#[test]
fn every_utf8_prefix_preserves_the_contract() {
    for source in [
        "",
        "a # tail",
        "{ [a] }",
        "[ {a\nb} ]",
        "$[1.5e+3-.5 >= 2 && true]",
        "echo \"é$x[k]猫\"",
        "echo \"!{return $[1+2]} tail\"",
        "##'a #'猫'##",
        "echo \"a\\\n$x z\"",
        "echo \"\\u{202E}\"",
        "echo \"$(name)[x]\"",
        "echo \"~/é\"",
    ] {
        for end in source
            .char_indices()
            .map(|(at, _)| at)
            .chain([source.len()])
        {
            check_contract(&source[..end]);
        }
    }
}

#[test]
fn short_source_exhaustively_preserves_the_contract() {
    let alphabet = [
        'a', 'é', '#', '\'', '"', '\\', '$', '!', '{', '}', '[', ']', '&', ';', '\n',
    ];
    for mut code in 0..alphabet.len().pow(4) {
        let source: String = (0..4)
            .map(|_| {
                let ch = alphabet[code % alphabet.len()];
                code /= alphabet.len();
                ch
            })
            .collect();
        check_contract(&source);
    }
}

#[test]
fn redirects_have_exactly_the_nine_spellings() {
    use RedirectOp::{HereString, Read, Stderr, Stdout};
    use WriteMode::{Append, Stream, Write};
    for (source, expected) in [
        ("< f", Token::Redirect(Read)),
        ("<< 's'", Token::Redirect(HereString)),
        ("> f", Token::Redirect(Stdout(Write))),
        (">> f", Token::Redirect(Stdout(Append))),
        (">~ f", Token::Redirect(Stdout(Stream))),
        ("2> f", Token::Redirect(Stderr(Write))),
        ("2>> f", Token::Redirect(Stderr(Append))),
        ("2>~ f", Token::Redirect(Stderr(Stream))),
        ("2>&1", Token::StderrToStdout),
    ] {
        assert_eq!(lex(source).unwrap()[0].token, expected, "{source:?}");
        check_contract(source);
    }
    for source in [
        "02> f", "002>> f", "02>~ f", "2>&01", "02>&1", "00< f", "01> f",
    ] {
        assert!(lex(source).is_err(), "{source:?}");
        assert!(lex_open(source).is_none(), "{source:?}");
    }
    assert!(parse("echo 02 > f").is_ok());
    assert!(parse("$[02>1]").is_ok());
}
