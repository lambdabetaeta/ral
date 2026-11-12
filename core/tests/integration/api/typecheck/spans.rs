//! Span pinning for typechecker diagnostics.
//!
//! Where the `tests/reject/` corpus checks the *wording* of diagnostics, this
//! checks the *position* — that the caret lands on the offending
//! sub-expression, not somewhere generic like the `return` keyword or the
//! start of the file.
//!
//! Each entry in [`span_cases`] is a [`SpanCase`] `{ src, expected, tag }`.
//! The test asserts that the type error's span, sliced out of `src`,
//! equals `expected`.  When a mistake produces multiple errors we only
//! pin the first one — that is the entry the user sees first and acts on.

use ral_core::typecheck::{TypeError, Unit};
use ral_core::{elaborator::elaborate, syntax::parser::parse, typecheck};

fn raw_errors(src: &str) -> Vec<TypeError> {
    let ast = parse(src).expect("parse");
    let comp = elaborate(&ast, [], None, &mut crate::common::no_modules).expect("elaborate");
    typecheck(
        &comp,
        crate::common::schemes_for(&ral_core::HostSurface::default()),
        Unit::Script,
    )
    .err()
    .unwrap_or_default()
}

fn slice_span(src: &str, err: &TypeError) -> Option<String> {
    let sp = err.pos?;
    let bytes = src.as_bytes();
    let start = (sp.start as usize).min(bytes.len());
    let end = (sp.end as usize).min(bytes.len()).max(start);
    Some(String::from_utf8_lossy(&bytes[start..end]).into_owned())
}

/// Each case: a source program and the *exact substring* the first
/// type error's caret should cover; keep it unique in the source, or the
/// case pins no location.
struct SpanCase {
    src: &'static str,
    expected: &'static str,
    /// Short tag so failures explain which case broke.
    tag: &'static str,
}

const fn c(src: &'static str, expected: &'static str, tag: &'static str) -> SpanCase {
    SpanCase { src, expected, tag }
}

fn span_cases() -> Vec<SpanCase> {
    vec![
        // Wrong-arg type to a known function: the caret must land on
        // the *argument*, not on the whole call — `arg_spans` on
        // `CompKind::App` lets `apply_args` narrow pos per position.
        c(
            "let f = { |x| return $[$x + 1] }\n!{f hello}",
            "hello",
            "wrong-arg-type",
        ),
        // Wrong-arg type to an Exec-dispatched builtin: same shape,
        // through the `Exec` arm of the dispatcher.  `Exec.arg_spans`
        // narrows pos to the offending arg too.
        c("let n = 42\nupper $n", "$n", "exec-wrong-arg-type"),
        // Branch-mismatch in an `if`: the second branch is what
        // disagrees with the first.  With `Spanned` on the Return
        // value, the caret narrows further — past the branch body —
        // to just the offending value expression.
        c(
            "if true { return 1 } else { return hello }",
            "hello",
            "if-branch-mismatch",
        ),
        // Non-bool condition: with `cond_span` on `Ast::If` and
        // `CompKind::If`, the caret narrows to just the condition
        // expression.  Use a non-1 cond literal so the expected
        // substring is unique in the source.
        c(
            "if 99 { return 1 } else { return 2 }",
            "99",
            "non-bool-cond",
        ),
        // Each `Prim::Index` keeps its key's span.
        c("let r = [a: 1]\nreturn $r[a][b]", "[b]", "field-on-int"),
        // The string in head position, not its arguments, is the offending value.
        c("'foo' bar baz", "'foo'", "string-as-head"),
        // Force a non-thunk: with `span` on `Ast::Force`, the caret
        // narrows to just the `!operand` extent.
        c(
            "let t = 42\nlet x = !$t\nreturn $x",
            "!$t",
            "force-non-thunk",
        ),
        // Control op in value position: the value `$try` is now
        // pinpointed by `Ast::Let.value_span`.
        c("let x = $try\nreturn $x", "$try", "ctrl-op-try"),
        // Case-arm payload mismatch: when the scrutinee has Integer
        // at `\`ok` but the handler body uses it as a String, the
        // caret lands on the argument whose use contradicts that payload.
        c(
            "let r = `ok 5\nlet x = case $r [`ok: { |s| !{upper $s} }, `err: { |_| !{upper hello} }]\nreturn $x",
            "$s",
            "case-arm-payload-mismatch",
        ),
        // Case-on-non-variant: with `scrutinee_span` on
        // `CompKind::Case`, the diagnostic narrows from "the whole
        // `case` form" to "just the scrutinee value".
        c(
            "let x = 1\nlet r = case $x [`ok: { |i| return $i }]\nreturn $r",
            "$x",
            "case-on-non-variant",
        ),
        // The caret now narrows past the literal to the offending element.
        c("return [1, hello]", "hello", "list-elem-narrowing"),
        c("let xs = [\"a\", 1]", "1", "list-elem-scalar"),
        c("let n = 42\nreturn [...$n]", "$n", "list-spread-operand"),
        c(
            "surface `card [`text [spans: [\"plain\", [role: \"ok\", text: \"y\"]]]]",
            "[role: \"ok\", text: \"y\"]",
            "card-span-list-elem",
        ),
        c(
            "surface `card [`text [spans: [[role: \"code\", text: \"x\"], [text: \"y\"]]]]",
            "[text: \"y\"]",
            "card-span-missing-role",
        ),
        c("within [dir: 7] { echo hi }", "7", "scope-option-value"),
        c(
            "let k = \"x\"\nlet m = [$k: 1, $k: \"two\"]",
            "\"two\"",
            "map-elem-value",
        ),
        c(
            "let n = 42\nlet m = [...$n, a: 1]",
            "$n",
            "record-spread-operand",
        ),
        // A bad stage *early* in a 3-stage pipeline must underline that
        // stage, not the last one.  The lambda still wants an argument, so
        // it is not a computation that can run; the caret lands on it rather
        // than on `upper`.
        c(
            "echo hi | !{ |x| echo $x } | upper",
            "!{ |x| echo $x }",
            "pipeline-stage-shape-early-stage",
        ),
    ]
}

#[test]
fn scope_option_errors_follow_written_order_not_key_order() {
    for form in ["within", "grant"] {
        let src = format!("{form} [zzz: 17, aaa: 23] {{ return () }}");
        let errs = raw_errors(&src);
        let spans: Vec<_> = errs.iter().map(|err| slice_span(&src, err)).collect();
        assert_eq!(spans, [Some("17".into()), Some("23".into())], "{src}");
        assert!(errs[0].message().contains("zzz"));
        assert!(errs[1].message().contains("aaa"));
    }
}

/// Pin each case's first error span to the expected substring.
#[test]
fn first_error_span_matches_expected() {
    let mut failures: Vec<String> = Vec::new();
    for case in span_cases() {
        let errs = raw_errors(case.src);
        let Some(err) = errs.first() else {
            failures.push(format!(
                "{}: produced no type error at all (source: {:?})",
                case.tag, case.src
            ));
            continue;
        };
        let Some(actual) = slice_span(case.src, err) else {
            failures.push(format!(
                "{}: error has no span (kind: {:?})",
                case.tag, err.kind
            ));
            continue;
        };
        if actual != case.expected {
            failures.push(format!(
                "{tag}: expected caret on {expected:?} but it landed on {actual:?}",
                tag = case.tag,
                expected = case.expected,
                actual = actual
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "span pins failed:\n  {}",
        failures.join("\n  ")
    );
}

/// Every type-error span is within the source, non-empty, and does not end
/// on a newline: catches a forgotten `with_span` leaking an unrelated `pos`.
/// The probes are the `tests/reject/` programs not pinned by [`span_cases`].
#[test]
fn every_error_span_is_well_formed() {
    let probes: &[&str] = &[
        "let x = hello\nreturn $[$x + 1]",
        "let x = 42\n$x foo",
        "let r = [a: 1, b: 2]\nreturn $r[missing]",
        "let xs = [1, 2, 3]\nreturn $xs[hello]",
        "let m = [a: 1, b: 2]\nreturn $m[0]",
        "let t = { return 42 }\nreturn $t[a]",
        "let [a, b] = 42\nreturn $a",
    ];
    for src in probes {
        let errs = raw_errors(src);
        assert!(!errs.is_empty(), "{src:?}: produced no type error");
        for err in errs {
            let sp = err
                .pos
                .unwrap_or_else(|| panic!("error from {src:?} has no span: {:?}", err.kind));
            let (start, end) = (sp.start as usize, sp.end as usize);
            assert!(
                start < end,
                "{src:?}: empty or inverted span [{start}, {end})"
            );
            assert!(end <= src.len(), "{src:?}: span end {end} past source");
            assert!(
                !src[start..end].ends_with('\n'),
                "{src:?}: span includes trailing newline"
            );
        }
    }
}
