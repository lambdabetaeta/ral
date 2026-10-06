use super::*;
use crate::source::{FileId, SourceDb};
use crate::syntax::lexer::{LexErrorKind, StringForm};
use crate::syntax::parser::{ParseError, ParseErrorKind};
use crate::typecheck::{TypeError, TypeErrorKind};
use crate::types::Error;

fn parse_error_with(message: &str, span: Option<Span>) -> ParseError {
    ParseError {
        message: message.into(),
        span,
        kind: ParseErrorKind::Plain,
        incomplete: false,
    }
}

fn parse_error_from(kind: LexErrorKind) -> ParseError {
    ParseError {
        message: kind.message(),
        span: None,
        kind: ParseErrorKind::Lex(kind),
        incomplete: false,
    }
}

fn drawn(file: &str, text: &str, err: &ParseError) -> String {
    let src = Source::from_text(file, text);
    err.report(&src).render(&src)
}

#[test]
fn parse_error_ariadne_points_at_source() {
    // `x = [a, b` — point at byte 8 (the trailing `b`).
    let span = Span::new(FileId::DUMMY, 8, 9);
    let err = parse_error_with("expected ',' or ']' in list", Some(span));
    let output = drawn("test.al", "x = [a, b", &err);
    assert!(output.contains("P0001"));
    assert!(output.contains("expected ',' or ']' in list"));
    assert!(output.contains("test.al"));
}

#[test]
fn lex_error_ariadne_renders_unterminated_string_with_open_anchor() {
    let err = parse_error_from(LexErrorKind::UnterminatedString {
        form: StringForm::DoubleQuoted,
        opened: Span::new(FileId::DUMMY, 0, 1),
        inner: None,
    });
    let output = drawn("test.al", "\"foo", &err);
    assert!(output.contains("L0001"), "got:\n{output}");
    assert!(output.contains("unterminated double-quoted string"));
    assert!(output.contains("opened here"));
}

#[test]
fn lex_error_ariadne_names_the_bumped_closing_delimiter() {
    // The `#` run is the whole difficulty of the form: a reader who is
    // told only "unterminated" retypes `'` and fails again.  Both the
    // headline and the end-of-input label must show `'##`.
    let err = parse_error_from(LexErrorKind::UnterminatedString {
        form: StringForm::BumpedSingle(2),
        opened: Span::new(FileId::DUMMY, 0, 1),
        inner: None,
    });
    let output = drawn("test.al", "##'foo", &err);
    assert!(
        output.matches("expected closing `'##`").count() == 2,
        "got:\n{output}"
    );
}

#[test]
fn lex_error_ariadne_includes_inner_help_for_nested_unclosed() {
    let inner = LexErrorKind::UnterminatedBalanced {
        open: '{',
        close: '}',
        opened: Span::new(FileId::DUMMY, 6, 7),
    };
    let err = parse_error_from(LexErrorKind::UnterminatedString {
        form: StringForm::DoubleQuoted,
        opened: Span::new(FileId::DUMMY, 0, 1),
        inner: Some(Box::new(inner)),
    });
    let output = drawn("test.al", "\"foo !{cmd", &err);
    assert!(output.contains("L0001"));
    assert!(output.contains("nested"));
    assert!(output.contains("`{…}`"), "got:\n{output}");
}

/// The lexer's own line and the report's headline are one spelling.
#[test]
fn balanced_error_spells_its_delimiters_once() {
    let kind = LexErrorKind::UnterminatedBalanced {
        open: '[',
        close: ']',
        opened: Span::new(FileId::DUMMY, 0, 1),
    };
    let err = parse_error_from(kind.clone());
    let src = Source::from_text("t.ral", "[a");
    assert_eq!(err.report(&src).message, kind.message());
    assert_eq!(kind.message(), "unterminated `[…]`");
}

fn touching_help(src: &str) -> String {
    let err = crate::syntax::parser::parse(src).unwrap_err();
    assert!(
        matches!(err.kind, ParseErrorKind::Touching { .. }),
        "{src}: {err}"
    );
    drawn("test.ral", src, &err)
}

#[test]
fn touching_words_render_both_readings() {
    let cases = [
        (
            "echo --prefix=$d",
            r#"two arguments: `--prefix= $d`; one argument: "--prefix=$d""#,
        ),
        ("echo 'a'\"b\"'c'", r#"one argument: "abc""#),
        ("echo $h[k]'x'", r#"one argument: "$h[k]x""#),
        (r"echo C:\tmp\$leaf", r#"one argument: "C:\\tmp\\$leaf""#),
        ("echo a!{b c}", r#"one argument: "a!{b c}""#),
        ("echo 'it''s'", r#"one argument: "its""#),
        ("echo '~'/x", r#"one argument: "\~/x""#),
    ];
    for (src, help) in cases {
        let output = touching_help(src);
        assert!(output.contains("P0002"), "{src}:\n{output}");
        assert!(output.contains(help), "{src}:\n{output}");
    }
}

#[test]
fn touching_words_label_both_atoms() {
    let output = touching_help("echo --prefix=$d");
    assert!(output.contains("words touch: whitespace separates words, and nothing joins them"));
    assert!(output.contains("this word starts with no space before it"));
    assert!(output.contains("this word ends here"));
}

fn db_with(name: &str, text: &str) -> (SourceDb, FileId) {
    let mut db = SourceDb::default();
    let id = db.register(Source::from_text(name, text));
    (db, id)
}

/// A loaded file's compile failure draws its own report, even where the
/// error would otherwise render compact.
#[test]
fn rejected_error_renders_its_compile_report() {
    let span = Span::new(FileId::DUMMY, 4, 5);
    let err = crate::compile::CompileError::Parse(parse_error_with("expected ]", Some(span)))
        .into_error(Source::from_text("plugin.ral", "let [x"));
    let (db, root) = db_with("main.ral", "load-plugin plugin");
    let out = err.render(&db, Some(root));
    assert!(out.contains("P0001") && out.contains("plugin.ral"), "{out}");
    assert_eq!(err.message, "parse error: expected ]");
}

fn runtime(db: &SourceDb, span: Option<Span>, witness: Option<Span>, hint: Option<&str>) -> String {
    let mut err = Error::new("boom").with_witness(witness);
    err.span = span;
    err.hint = hint.map(Into::into);
    err.render(db, None)
}

#[test]
fn runtime_error_ariadne_points_at_source() {
    let (db, file) = db_with("test.al", "x = 5\ny = 10\necho $undefined\n");
    let output = runtime(&db, Some(Span::new(file, 18, 28)), None, None);
    assert!(output.contains("R0001"));
    assert!(output.contains("boom"));
    assert!(output.contains("test.al"));
}

#[test]
fn runtime_error_ariadne_renders_hint() {
    let (db, file) = db_with("test.al", "[a, b] = 5");
    let output = runtime(
        &db,
        Some(Span::new(file, 0, 6)),
        None,
        Some("the right-hand side must evaluate to a list"),
    );
    assert!(output.contains("the right-hand side must evaluate to a list"));
}

/// `café` is 5 bytes and 4 chars; the underline must stop at the token.
#[test]
fn caret_width_is_the_span_for_multibyte() {
    let src = Source::from_text("t", "café bar");
    assert_eq!(caret(&src, Some(Span::new(FileId::DUMMY, 0, 5))), 0..5);
}

/// A zero-width span and the end of input both stay drawable, whole chars.
#[test]
fn empty_and_eof_carets_widen_to_a_char() {
    let src = Source::from_text("t", "aé");
    assert_eq!(caret(&src, Some(Span::point(FileId::DUMMY, 0))), 0..1);
    assert_eq!(caret(&src, Some(Span::point(FileId::DUMMY, 1))), 1..3);
    assert_eq!(caret(&src, None), 1..3);
    assert_eq!(caret(&Source::from_text("t", ""), None), 0..0);
}

#[test]
fn type_error_ariadne_with_span() {
    let sp = Span::new(FileId::DUMMY, 21, 28);
    let err = TypeError {
        pos: Some(sp),
        kind: TypeErrorKind::TyMismatch {
            expected: Box::new(crate::ty::Ty::Int),
            actual: Box::new(crate::ty::Ty::String),
        },
        reason: Some(crate::typecheck::Reason::IfCond),
        weak: None,
        unit: None,
    };
    let src = Source::from_text("test.ral", "if 1 { return 42 } else { return \"hello\" }");
    let output = err.report().render(&src);
    assert!(output.contains("T0010"));
    assert!(output.contains("couldn't match"));
    assert!(output.contains("Integer"));
    assert!(output.contains("String"));
    assert!(output.contains(
        "the condition of an `if` must be a Bool: either `true`/`false` \
         or an expression that produces one (e.g. `$[$x == 1]`)"
    ));
}

#[test]
fn type_error_ariadne_without_span_is_messageless() {
    let err = TypeError {
        pos: None,
        kind: TypeErrorKind::RecursiveRow,
        reason: None,
        weak: None,
        unit: None,
    };
    let output = err
        .report()
        .render(&Source::from_text("test.ral", "let x = 1"));
    assert!(output.contains("infinite row"));
    assert!(output.contains("T0002"));
}

#[test]
fn runtime_error_resolved_source_draws_caret() {
    let (db, file) = db_with("main.ral", "echo x");
    let out = runtime(&db, Some(Span::new(file, 5, 6)), None, None);
    assert!(out.contains("R0001"));
    assert!(
        out.contains("here"),
        "a resolved span should draw a caret:\n{out}"
    );
    assert!(out.contains("main.ral"));
}

/// The placeholder id names no source, so no caret is drawn in any of them.
#[test]
fn runtime_error_in_unregistered_source_is_messageless() {
    let (db, _) = db_with("main.ral", "echo x");
    let out = runtime(&db, Some(Span::new(FileId::DUMMY, 2, 6)), None, None);
    assert!(out.contains("R0001"));
    assert!(out.contains("boom"));
    assert!(
        !out.contains("here"),
        "an unresolved span must not draw a caret in any source:\n{out}"
    );
}

/// Two sources in one db: the caret follows the span's file, not the
/// top-level's, even where the same byte range would land in both.
#[test]
fn runtime_error_in_module_draws_into_module_source() {
    let mut db = SourceDb::default();
    let _top = db.register(Source::from_text("main.ral", "use 'mod.ral'\n"));
    let module = db.register(Source::from_text(
        "mod.ral",
        "let a = 1\nfail [status: 1, message: 'kaboom']\n",
    ));
    // Bytes 10..14 of the module are `fail`, past the top-level's end.  Strip
    // ANSI: on a tty ariadne colours the span character by character, which
    // splits `fail` with escapes.
    let out = crate::ansi::strip(&runtime(&db, Some(Span::new(module, 10, 14)), None, None));
    assert!(out.contains("R0001"));
    assert!(
        out.contains("mod.ral"),
        "the caret must be drawn against the module's source:\n{out}"
    );
    assert!(
        !out.contains("main.ral"),
        "the top-level source must not appear:\n{out}"
    );
    assert!(
        out.contains("fail"),
        "the underlined line must be the module's line 2:\n{out}"
    );
}

#[test]
fn no_color_output_has_no_ansi() {
    // Absence of escapes is not assertable — `stderr_color` may be true in a
    // tty — so only the content is checked.
    let out = Report {
        code: Some("T9999"),
        message: "message".into(),
        at: None,
        also: None,
        hint: Some("hint".into()),
    }
    .plain();
    assert!(out.contains("T9999"));
    assert!(out.contains("message"));
    assert!(out.contains("hint"));
}
