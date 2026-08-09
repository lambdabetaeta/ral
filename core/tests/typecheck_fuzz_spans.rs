//! Span pinning for typechecker diagnostics.
//!
//! Companion to `typecheck_fuzz.rs`.  Where that file checks the
//! *wording* of diagnostics, this one checks the *position* — that the
//! caret lands on the offending sub-expression, not somewhere generic
//! like the `return` keyword or the start of the file.
//!
//! Each entry in [`span_cases`] is a [`SpanCase`] `{ src, expected, tag }`.
//! The test asserts that the type error's span, sliced out of `src`,
//! equals `expected`.  When a mistake produces multiple errors we only
//! pin the first one — that is the entry the user sees first and acts on.

mod common;

use std::collections::HashSet;

use ral_core::typecheck::TypeError;
use ral_core::{elaborator::elaborate, syntax::parser::parse, typecheck};

fn raw_errors(src: &str) -> Vec<TypeError> {
    let Ok(ast) = parse(src) else {
        return Vec::new();
    };
    let comp = elaborate(&ast, HashSet::default(), "").expect("elaborate");
    typecheck(
        &comp,
        ral_core::SessionSchemes::from_schemes(
            common::prelude_schemes(),
            ral_core::HostSurface::default().builtin_table(),
        ),
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
/// type error's caret should cover.  The substring must be unique in
/// the source — anything else suggests a test that doesn't actually
/// pin the location it claims to.
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
        // Field on Int via index chain: `$r[a][b]` — the second `[b]`
        // is on an Int.  `key_spans` on `CompKind::Index` narrows the
        // caret onto exactly that step in the chain.
        c("let r = [a: 1]\nreturn $r[a][b]", "[b]", "field-on-int"),
        // String as command head: the whole call carries a Call span.
        c("'foo' bar baz", "'foo' bar baz", "string-as-head"),
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
        // caret must land on the *offending arm's body*, not on the
        // whole `let r = case … [` … `]` form.
        c(
            "let r = `ok 5\nlet x = case $r [`ok: { |s| !{upper $s} }, `err: { |_| !{upper hello} }]\nreturn $x",
            "!{upper $s}",
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
        // Return value: with `Spanned` on `Ast::Return`'s payload the
        // caret narrows from the whole `return …` statement to just
        // the value expression — `[1, hello]`, here.
        c("return [1, hello]", "[1, hello]", "return-value-narrowing"),
        // A mode mismatch on an *early* edge of a 3-stage pipeline must
        // underline whichever end of that edge is the actual violator, not
        // the last stage.  `'abc'` is a value producer where the edge to
        // `from-json` needs Bytes, so the caret lands on `'abc'`, not
        // `from-json` or `upper`.
        c(
            "'abc' | from-json | upper",
            "'abc'",
            "pipeline-mode-mismatch-early-edge",
        ),
    ]
}

/// Compute how many times `needle` appears in `haystack` (non-overlapping).
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut rest = haystack;
    while let Some(idx) = rest.find(needle) {
        count += 1;
        rest = &rest[idx + needle.len()..];
    }
    count
}

/// Each case's `expected` substring must be unique in the source —
/// otherwise the assertion below doesn't actually pin the span, it
/// only asserts equality against one of several plausible matches.
#[test]
fn span_case_expectations_are_unique() {
    for case in span_cases() {
        let n = count_occurrences(case.src, case.expected);
        assert!(
            n >= 1,
            "case {:?}: expected substring {:?} does not appear in source {:?}",
            case.tag,
            case.expected,
            case.src,
        );
        // We allow ambiguity only when the expected text *is* the
        // whole source — there's nowhere else it could land.
        if case.expected != case.src {
            assert!(
                n == 1,
                "case {:?}: expected substring {:?} is ambiguous in source {:?} ({n} matches)",
                case.tag,
                case.expected,
                case.src,
            );
        }
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

/// Property: every type-error span must be a strict (or equal) subspan
/// of the source — start ≤ end, both within `[0, source.len()]`, and
/// the substring it slices must not span a trailing newline.  This
/// catches regressions where `with_span` is forgotten and pos leaks an
/// unrelated value, or where a span ends up out-of-bounds after some
/// transformation.
///
/// We don't (yet) have a "subspan of *the enclosing Comp*" check — that
/// would need a Comp-side traversal to compute the parent span for
/// each error.  This weaker invariant is the cheap proxy.
#[test]
fn span_is_well_formed_for_every_scenario() {
    let probes: &[&str] = &[
        "let x = hello\nreturn $[$x + 1]",
        "if 99 { return 1 } else { return hello }",
        "return [1, hello]",
        "'foo' bar baz",
        "let x = 42\n$x foo",
        "let f = { |x| return $[$x + 1] }\n!{f hello}",
        "let r = [a: 1]\nreturn $r[a][b]",
        "let r = [a: 1, b: 2]\nreturn $r[missing]",
        "let xs = [1, 2, 3]\nreturn $xs[hello]",
        "let m = [a: 1, b: 2]\nreturn $m[0]",
        "let t = 42\nlet x = !$t\nreturn $x",
        "let t = { return 42 }\nreturn $t[a]",
        "let x = $try\nreturn $x",
        "let [a, b] = 42\nreturn $a",
        "let n = 42\nreturn [...$n]",
        "let n = 42\nkeys $n",
    ];
    for src in probes {
        let len = src.len();
        for err in raw_errors(src) {
            let Some(sp) = err.pos else {
                panic!("error from {src:?} has no span: {:?}", err.kind);
            };
            let start = sp.start as usize;
            let end = sp.end as usize;
            assert!(start <= end, "{src:?}: span start {start} > end {end}");
            assert!(
                end <= len,
                "{src:?}: span end {end} past source length {len}"
            );
            assert!(end > start, "{src:?}: zero-width span [{start}, {end})");
            let text = &src[start..end];
            assert!(
                !text.ends_with('\n'),
                "{src:?}: span includes trailing newline: {text:?}"
            );
        }
    }
}
