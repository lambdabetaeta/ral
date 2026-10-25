//! A parse failure as a [`Report`].

use super::lexer::LexErrorKind;
use super::parser::{EntryReading, ParseError, ParseErrorKind};
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
        let plain = |hint: Option<&String>| Report {
            code: Some("P0001"),
            message: self.message.clone(),
            at: Some(label(self.span, "here")),
            also: None,
            hint: hint.cloned(),
        };
        match &self.kind {
            ParseErrorKind::Lex(kind) => {
                lex_report(src, kind, self.span).unwrap_or_else(|| plain(None))
            }
            ParseErrorKind::Touching { first, second, run } => {
                self.touching(src, *first, *second, *run)
            }
            ParseErrorKind::EntryRuns {
                entry,
                head,
                reading,
            } => self.entry_runs(src, *entry, *head, reading),
            ParseErrorKind::Plain { hint } => plain(hint.as_ref()),
        }
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

    /// An entry that runs past its value: a command for `!{…}`, or a missing
    /// `,`, which leads when a newline follows the value.
    fn entry_runs(&self, src: &Source, entry: Span, head: Span, reading: &EntryReading) -> Report {
        let text = |span: Span| &src.as_str()[span.range()];
        let comma = "if these are separate entries, put `,` between them";
        let gap = &src.as_str()[head.end as usize..entry.end as usize];
        let broken = gap
            .chars()
            .take_while(|c| c.is_whitespace())
            .any(|c| c == '\n');
        let hint = match reading {
            EntryReading::Statement(word) => format!(
                "`{word}` begins a statement: write `!{{{word} …}}`, or bind it first with `let`"
            ),
            EntryReading::Binding => {
                "a `let` binds for the statements after it, not inside an entry: \
                 write the `let` before this collection, and put its name here"
                    .into()
            }
            EntryReading::Command if broken => {
                format!("{comma}; or, to run a command, write the entry as `!{{…}}`")
            }
            EntryReading::Command => format!(
                "`{e}` runs a command: write `!{{{e}}}`, or, {comma}",
                e = text(entry)
            ),
            EntryReading::Value => comma.into(),
        };
        let (at, also) = match reading {
            EntryReading::Statement(_) | EntryReading::Binding => (
                label(Some(entry), "this entry is a statement, not a value"),
                None,
            ),
            EntryReading::Command | EntryReading::Value => (
                label(Some(entry), "this entry holds more than one value"),
                Some(label(Some(head), "its first value ends here")),
            ),
        };
        Report {
            code: Some("P0003"),
            message: self.message.clone(),
            at: Some(at),
            also,
            hint: Some(hint),
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
        LexErrorKind::Mismatched { .. } | LexErrorKind::Other(_) => {
            "an unrelated lexer error".into()
        }
    }
}

/// `None` for `Other(_)`, so the caller falls back to the single-label form.
/// `at` is where the lexer stopped.  Codes are never reused: the next lex
/// diagnostic takes L0007.
fn lex_report(src: &Source, kind: &LexErrorKind, at: Option<Span>) -> Option<Report> {
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
        LexErrorKind::Mismatched {
            open,
            opened,
            close,
        } => Report {
            code: Some("L0006"),
            message: kind.headline(),
            at: Some(label(at, format!("`{close}` closes nothing"))),
            also: Some(label(Some(*opened), format!("`{open}` opened here"))),
            hint: None,
        },
        LexErrorKind::Other(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{FileId, SourceFile, SourceOrigin};
    use crate::syntax::lexer::StringForm;

    fn file_origin(path: &str) -> SourceOrigin {
        SourceOrigin::File(SourceFile::new(path).expect("UTF-8"))
    }

    fn parse_error_with(message: &str, span: Option<Span>) -> ParseError {
        ParseError {
            message: message.into(),
            span,
            kind: ParseErrorKind::Plain { hint: None },
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
        let src = Source::from_text(file_origin(file), text);
        err.report(&src).render(&src)
    }

    #[test]
    fn parse_error_ariadne_points_at_source() {
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

    #[test]
    fn balanced_error_spells_its_delimiters_once() {
        let kind = LexErrorKind::UnterminatedBalanced {
            open: '[',
            close: ']',
            opened: Span::new(FileId::DUMMY, 0, 1),
        };
        let err = parse_error_from(kind.clone());
        let src = Source::from_text(file_origin("t.ral"), "[a");
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
    fn an_entry_that_runs_renders_its_readings() {
        for (src, help) in [
            (
                "[str $x[type], $x[size]]",
                "Help: `str $x[type]` runs a command: write `!{str $x[type]}`, or, if these are \
                 separate entries, put `,` between them",
            ),
            (
                "[k: if $c { 1 } else { 2 }]",
                "Help: `if` begins a statement: write `!{if …}`, or bind it first with `let`",
            ),
            (
                "[1 2]",
                "Help: if these are separate entries, put `,` between them",
            ),
            (
                "[\n  str\n  $x\n]",
                "Help: if these are separate entries, put `,` between them; or, to run",
            ),
        ] {
            let err = crate::syntax::parser::parse(src).unwrap_err();
            let output = drawn("test.ral", src, &err);
            assert!(output.contains("P0003"), "{src}:\n{output}");
            assert!(output.contains(help), "{src}:\n{output}");
        }
    }

    #[test]
    fn a_statement_entry_is_not_labelled_a_value() {
        for src in ["[k: if $c { 1 } else { 2 }]", "[k: let x = 1]"] {
            let err = crate::syntax::parser::parse(src).unwrap_err();
            let output = drawn("test.ral", src, &err);
            assert!(
                output.contains("this entry is a statement, not a value"),
                "{src}:\n{output}"
            );
            assert!(
                !output.contains("its first value ends here"),
                "{src}:\n{output}"
            );
        }
    }

    #[test]
    fn touching_words_label_both_atoms() {
        let output = touching_help("echo --prefix=$d");
        assert!(output.contains("words touch: whitespace separates words, and nothing joins them"));
        assert!(output.contains("this word starts with no space before it"));
        assert!(output.contains("this word ends here"));
    }
}
