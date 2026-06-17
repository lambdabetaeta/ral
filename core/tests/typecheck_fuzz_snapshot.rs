//! One-shot snapshot of the typechecker's error output across a curated
//! catalogue of mistakes.  The point is *human* review: run with
//! `RAL_PRINT_SNAPSHOTS=1` and read the messages with a first-year
//! undergraduate sat next to you, asking "would they understand this?".
//!
//! The test passes as long as the typechecker doesn't panic.

mod common;

use ral_core::diagnostic::format_type_error_ariadne;
use ral_core::typecheck::{TypeError, fmt_ty};
use ral_core::{elaborate, parse, typecheck};

fn raw_errors(src: &str) -> Vec<TypeError> {
    let ast = match parse(src) {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };
    let comp = elaborate(&ast, Default::default());
    typecheck(
        &comp,
        ral_core::SessionSchemes::from_schemes(common::prelude_schemes()),
    )
    .err()
    .unwrap_or_default()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && matches!(chars.peek(), Some(&'[')) {
            chars.next();
            while let Some(&c) = chars.peek() {
                chars.next();
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[test]
fn dump_snapshots() {
    if std::env::var("RAL_PRINT_SNAPSHOTS").is_err() {
        return;
    }
    let cases: &[(&str, &str)] = &[
        ("int+string", "let x = hello\nreturn $[$x + 1]"),
        ("if-branch", "if true { return 1 } else { return hello }"),
        ("non-bool-cond", "if 1 { return 1 } else { return 2 }"),
        ("hetero-list", "return [1, hello]"),
        ("string-as-head", "'foo' bar baz"),
        ("int-as-head", "let x = 42\n$x foo"),
        ("wrong-arg", "let f = { |x| return $[$x + 1] }\n!{f hello}"),
        ("missing-field", "let r = [a: 1, b: 2]\nreturn $r[missing]"),
        (
            "absent-field-in-arg",
            "let r = [a: 1]\nlet take = { |x| return $x[b] }\nreturn !{take $r}",
        ),
        ("field-on-int", "let r = [a: 1]\nreturn $r[a][b]"),
        (
            "case-missing",
            "let r = if true { return `ok 1 } else { return `err hello }\nlet x = case $r [`ok: { |i| return $i }]\nreturn $x",
        ),
        (
            "case-arm-mismatch",
            "let r = `ok 5\nlet x = case $r [`ok: { |s| !{upper $s} }, `err: { |_| !{upper hello} }]\nreturn $x",
        ),
        (
            "variant-payload",
            "let a = `ok 1\nlet b = `ok hello\nreturn [$a, $b]",
        ),
        ("list-string-index", "let xs = [1, 2, 3]\nreturn $xs[hello]"),
        ("record-int-index", "let m = [a: 1, b: 2]\nreturn $m[0]"),
        ("force-non-thunk", "let t = 42\nlet x = !$t\nreturn $x"),
        ("index-thunk", "let t = { return 42 }\nreturn $t[a]"),
        ("ctrl-op-try", "let x = $try\nreturn $x"),
        ("list-pattern-on-int", "let [a, b] = 42\nreturn $a"),
        (
            "case-on-int",
            "let x = 1\nlet r = case $x [`ok: { |i| return $i }]\nreturn $r",
        ),
        ("not-on-int", "let x = 1\nreturn $[not $x]"),
        ("spread-non-list-in-literal", "let n = 42\nreturn [...$n]"),
        (
            "if-branch-row-mismatch",
            "if true { return [a: 1] } else { return [b: 2] }",
        ),
        (
            "compare-int-string",
            "let a = 1\nlet h = hello\nreturn $[$a < $h]",
        ),
        (
            "nested-error-in-lambda-body",
            "let h = hello\nlet f = { |x| return $[$x + $h] }\nreturn !{f 1}",
        ),
    ];
    for (tag, src) in cases {
        let errs = raw_errors(src);
        eprintln!("\n────────── {tag} ──────────");
        eprintln!("{src}");
        eprintln!("──── diagnostics ────");
        for e in &errs {
            eprintln!("KIND: {} (code {})", e.kind.render_message(), e.kind.code());
            if let Some(h) = &e.hint {
                eprintln!("HINT: {h}");
            }
        }
        eprintln!("──── ariadne render ────");
        for e in &errs {
            eprintln!(
                "{}",
                strip_ansi(&format_type_error_ariadne("fuzz.ral", src, e))
            );
        }
        // Also explicitly show the type-pretty-printing for plain types
        // so we can review the noun choices.
        let _ = fmt_ty;
    }
}
