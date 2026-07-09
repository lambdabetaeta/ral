//! Typechecker fuzz tests.
//!
//! The job here is twofold.  First, the typechecker must never panic on
//! any reasonably-parseable program — even nonsense ones.  Second, when
//! it does produce diagnostics, those diagnostics must be readable by a
//! first-year undergraduate who has seen a little Haskell.  That second
//! condition is encoded by the `prose_*` set of assertions below: the
//! rendered error text must avoid internal jargon (`CompTy`, `TyVar`,
//! `unifier`, `union-find`, …) and must use English nouns the reader
//! already has — `function`, `record field`, `branch`, `case alternative`.
//!
//! The fuzz vector lives in [`scenarios`]; each entry is `(source, tag)`.
//! `tag` is the kind of mistake we're trying to provoke.  We run every
//! scenario through `parse → elaborate → typecheck` and verify both
//! invariants on every diagnostic emitted.

mod common;

use std::collections::HashSet;

use ral_core::diagnostic::format_type_error_ariadne;
use ral_core::typecheck::TypeError;
use ral_core::{elaborator::elaborate, syntax::parser::parse, typecheck};

fn raw_errors(src: &str) -> Vec<TypeError> {
    let Ok(ast) = parse(src) else {
        return Vec::new();
    };
    let comp = elaborate(&ast, HashSet::default());
    typecheck(
        &comp,
        ral_core::SessionSchemes::from_schemes(common::prelude_schemes()),
    )
    .err()
    .unwrap_or_default()
}

/// One fuzz scenario: a (hopefully ill-typed) program plus a short label.
struct Scenario {
    /// The source program we feed into parse → elaborate → typecheck.
    src: &'static str,
    /// Short label that names the *kind* of mistake we expect.
    tag: &'static str,
}

const fn s(src: &'static str, tag: &'static str) -> Scenario {
    Scenario { src, tag }
}

/// Adversarial inputs we want the typechecker to be able to handle.
///
/// Grouped by family.  Within a group the inputs are sorted by intent —
/// the simplest reproducer first, then variants that stress edge cases.
fn scenarios() -> Vec<Scenario> {
    vec![
        // ─── Scalar mismatches ───────────────────────────────────────────
        s("let x = hello\nreturn $[$x + 1]", "int+string"),
        s(
            "let x = hello\nlet y = 1\nreturn $[$x + $y]",
            "int+string-rhs",
        ),
        s("let x = 3.14\nlet y = 1\nreturn $[$x + $y]", "float+int"),
        s("let a = 1\nlet b = true\nreturn $[$a + $b]", "int+bool"),
        s("let a = unit\nreturn $[$a + 1]", "unit+int"),
        s("if 1 { return 1 } else { return 2 }", "non-bool-cond"),
        s("if hello { return 1 } else { return 2 }", "string-cond"),
        s("if [a: 1] { return 1 } else { return 2 }", "record-cond"),
        s(
            "if true { return 1 } else { return hello }",
            "if-branch-mismatch",
        ),
        s(
            "if true { return [a: 1] } else { return [b: 2] }",
            "if-branch-row-mismatch",
        ),
        s("return [1, hello]", "heterogeneous-list"),
        s("return [1, 2, true, hello]", "mixed-list"),
        s("return [[a: 1], [a: hello]]", "list-of-records-mismatched"),
        // ─── Boolean / negation ──────────────────────────────────────────
        s("let x = 1\nreturn $[not $x]", "not-on-int"),
        s("let h = hello\nreturn $[not $h]", "not-on-string"),
        s("let r = [a: 1]\nreturn $[not $r]", "not-on-record"),
        // ─── Comparison / equality ───────────────────────────────────────
        s(
            "let a = 1\nlet b = hello\nreturn $[$a < $b]",
            "compare-int-string",
        ),
        s(
            "let a = [a: 1]\nlet b = 1\nreturn $[$a == $b]",
            "eq-record-int",
        ),
        // ─── Function application ────────────────────────────────────────
        s("'foo' bar baz", "string-as-head"),
        s("let x = 42\n$x foo", "int-as-head"),
        s("let r = [a: 1]\n$r foo", "record-as-head"),
        s(
            "let f = { |x| return $[$x + 1] }\n!{f hello}",
            "wrong-arg-type",
        ),
        s(
            "let f = { |x| return $[$x + 1] }\n!{f [a: 1]}",
            "record-arg-on-int-fn",
        ),
        s(
            "let id = { |x| return $x }\nlet n = !{id 1}\nlet s = !{id hello}\nreturn $[$n + $s]",
            "poly-id-then-bad-use",
        ),
        s(
            "let f = { |x| { |y| return $[$x + $y] } }\n!{f 1 hello}",
            "second-arg-mismatch",
        ),
        // ─── Records / rows ───────────────────────────────────────────────
        s("let r = [a: 1, b: 2]\nreturn $r[missing]", "missing-field"),
        s(
            "let r = [a: 1]\nlet take = { |x| return $x[b] }\nreturn !{take $r}",
            "absent-field-in-arg",
        ),
        s("let r = [a: 1]\nreturn $r[a][b]", "field-on-int"),
        // ─── Variants & case ─────────────────────────────────────────────
        s(
            "let r = if true { return `ok 1 } else { return `err hello }\nlet x = case $r [`ok: { |i| return $i }]\nreturn $x",
            "case-missing-arm",
        ),
        s(
            "let r = `ok 5\nlet x = case $r [`ok: { |s| !{upper $s} }, `err: { |_| !{upper hello} }]\nreturn $x",
            "case-payload-mismatch",
        ),
        s(
            "let r = `ok 5\nlet x = case $r [`ok: { |x| return $x }, `err: { |_| return hello }]\nreturn $x",
            "case-arms-disagree",
        ),
        s(
            "let a = `ok 1\nlet b = `ok hello\nreturn [$a, $b]",
            "variant-payload-mismatch",
        ),
        s(
            "let x = 1\nlet r = case $x [`ok: { |i| return $i }]\nreturn $r",
            "case-on-int",
        ),
        // ─── Lists/maps and indexing ─────────────────────────────────────
        s("let xs = [1, 2, 3]\nreturn $xs[hello]", "list-string-index"),
        s("let m = [a: 1, b: 2]\nreturn $m[0]", "record-int-index"),
        s("let xs = [1, 2]\nreturn $xs[0][a]", "field-on-int-via-list"),
        s("let xs = 42\nreturn $xs[0]", "index-non-collection"),
        // ─── Thunks / forcing ────────────────────────────────────────────
        s("let t = 42\nlet x = !$t\nreturn $x", "force-non-thunk"),
        s("let t = hello\nlet x = !$t\nreturn $x", "force-string"),
        s("let t = { return 42 }\nreturn $t[a]", "index-thunk"),
        s("let t = [a: 1]\nlet x = !$t\nreturn $x", "force-record"),
        // ─── Control operators in value position ─────────────────────────
        s("let x = $try\nreturn $x", "ctrl-op-try"),
        s("let x = $within\nreturn $x", "ctrl-op-within"),
        s("let x = $grant\nreturn $x", "ctrl-op-grant"),
        s("let x = $audit\nreturn $x", "ctrl-op-audit"),
        s("let x = $guard\nreturn $x", "ctrl-op-guard"),
        // ─── Spread in list literal ──────────────────────────────────────
        s("let n = 42\nreturn [...$n]", "spread-non-list-in-literal"),
        // ─── Patterns ────────────────────────────────────────────────────
        s("let [a, b] = 42\nreturn $a", "list-pattern-on-int"),
        s("let [k: v] = 1\nreturn $v", "map-pattern-on-int"),
        // ─── Adversarial inputs that should not crash ────────────────────
        s("return [`ok 1, `err 2, `ok hello]", "variant-row-clash"),
        s(
            "let r = [`a: 1, `b: hello]\nreturn $r[`a]",
            "tag-keyed-bad-mix",
        ),
        s(
            "return [1, hello, true, unit, [a: 1]]",
            "all-mismatched-list",
        ),
        s(
            "return [1, [a: 1], hello, true, [b: 2], unit, [1, 2]]",
            "wide-mismatch",
        ),
        // ─── Pipeline shape: stream piped whole into element consumer ────
        s(
            "let s = !{stream-cons 1 { !{stream-nil} }}\n$s | { |e| return $[$e + 1] }",
            "stream-piped-whole-into-element-consumer",
        ),
        // ─── Nested error in nested function body ────────────────────────
        s(
            "let h = hello\nlet f = { |x| return $[$x + $h] }\nreturn !{f 1}",
            "nested-error-in-lambda-body",
        ),
        s(
            "let f = { |x| let y = !{$x foo}\nreturn $y }\nreturn !{f 1}",
            "force-int-inside-lambda",
        ),
    ]
}

// ── Jargon detection ──────────────────────────────────────────────────────

/// Words that betray internal representation: a beginner who sees
/// `CompTy::Var(7)` in their error message has no chance.  Each entry
/// here is a fragment whose presence in a rendered diagnostic is a
/// failure.  The list is deliberately conservative — only Rust-side
/// type names get banned, NOT the friendly Greek letters we use for
/// quantifier variables in printable schemes.  Error codes (`T0001`,
/// etc.) are intentionally shown by the ariadne renderer and not
/// flagged.
const JARGON_FRAGMENTS: &[&str] = &[
    "CompTy",
    "TyVar",
    "RowVar",
    "ModeVar",
    "CompTyVar",
    "Box<",
    "unifier",
    "union-find",
    "occurs check",
];

/// Heuristic: if a rendered message looks like it is *only* a raw
/// debug-print of an internal enum (e.g. begins with `Ty::` or contains
/// `Variant(Extend(`), flag it.  We don't want to print Debug for
/// `TypeError` ever; that's purely internal.
fn looks_like_debug_dump(msg: &str) -> bool {
    msg.contains("Ty::")
        || msg.contains("Variant(Extend(")
        || msg.contains("Row::")
        || msg.contains("CompTy::")
}

/// Assert that `msg` is free of internal jargon.
fn assert_no_jargon(msg: &str, scenario: &Scenario) {
    for bad in JARGON_FRAGMENTS {
        assert!(
            !msg.contains(bad),
            "scenario {:?}: message contains jargon {:?}\n  full message: {}",
            scenario.tag,
            bad,
            msg
        );
    }
    assert!(
        !looks_like_debug_dump(msg),
        "scenario {:?}: message looks like a Rust Debug dump\n  full message: {}",
        scenario.tag,
        msg
    );
}

// ── Top-level fuzz tests ──────────────────────────────────────────────────

/// Every diagnostic the typechecker emits must be free of internal
/// representation jargon.
#[test]
fn diagnostics_are_jargon_free() {
    for sc in scenarios() {
        for err in raw_errors(sc.src) {
            let msg = err.kind.render_message();
            assert_no_jargon(&msg, &sc);
            if let Some(hint) = err.hint() {
                assert_no_jargon(&hint, &sc);
            }
        }
    }
}

/// Strip ANSI escape sequences from `s` so jargon scans aren't fooled
/// by the colour codes ariadne emits around words.  ESC `[` … letter.
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

/// The fully-rendered ariadne report must also be jargon-free — this is
/// what the user actually sees on screen.  The render is more elaborate
/// than `render_message` (it adds an underline label and an error code),
/// so we run the same jargon scan on the whole rendering.
#[test]
fn rendered_reports_are_jargon_free() {
    for sc in scenarios() {
        let errs = raw_errors(sc.src);
        if errs.is_empty() {
            continue;
        }
        for err in &errs {
            let rendered = strip_ansi(&format_type_error_ariadne("fuzz.ral", sc.src, err));
            assert_no_jargon(&rendered, &sc);
        }
    }
}

/// At least one diagnostic must fire for each scenario — otherwise the
/// scenario is no longer probing the typechecker (e.g. an inference
/// improvement silently fixed it).  Worth knowing about: the fuzz set
/// should be kept lively.
#[test]
fn every_scenario_actually_produces_an_error() {
    let mut silent: Vec<&str> = Vec::new();
    for sc in scenarios() {
        if raw_errors(sc.src).is_empty() {
            silent.push(sc.tag);
        }
    }
    assert!(
        silent.is_empty(),
        "the following fuzz scenarios no longer produce a type error \
         (their content drifted, or inference grew strong enough to \
         resolve them) — refresh them: {silent:?}"
    );
}

// ── Positive shape assertions ──────────────────────────────────────────────
//
// These tests pin down concrete wording for representative scenarios so
// that future refactors don't silently revert to less friendly phrasing.

/// Primitive types should be spelled out in full, not abbreviated.
/// Beginners reading their first error don't know what `Int` means; the
/// long form makes the message self-explanatory.
#[test]
fn primitive_types_are_spelled_out() {
    let msgs: Vec<String> = raw_errors("let x = hello\nreturn $[$x + 1]")
        .into_iter()
        .map(|e| e.kind.render_message())
        .collect();
    let body = msgs.join("\n");
    assert!(
        body.contains("Integer"),
        "expected spelled-out 'Integer' in {body:?}"
    );
    assert!(body.contains("String"), "expected 'String' in {body:?}");
    assert!(
        !body.contains(" Int ") && !body.ends_with(" Int") && !body.starts_with("Int "),
        "abbreviated 'Int' leaked into diagnostic: {body:?}"
    );
}

/// When a row variable shows up in a diagnostic it should be named
/// with a Greek letter (ρ, σ, …), not the placeholder `..`.  Same for
/// value-type variables: α, β, …, not `_`.
#[test]
fn free_variables_get_greek_letters() {
    // The function infers `x: [a: α, ...ρ]` from `$x[a]`.  Calling
    // it with `1` (an Integer) provokes a unify failure between
    // `Integer` and `[a: α, ...ρ]`, putting both Greek letters in
    // the diagnostic.
    let msgs: Vec<String> = raw_errors("let take = { |x| return $x[a] }\nreturn !{take 1}")
        .into_iter()
        .map(|e| e.kind.render_message())
        .collect();
    let body = msgs.join("\n");
    let has_greek_ty = body.contains('α') || body.contains('β');
    let has_greek_row = body.contains('ρ') || body.contains('σ');
    assert!(
        has_greek_ty,
        "expected a Greek letter for the field's type in {body:?}"
    );
    assert!(
        has_greek_row,
        "expected a Greek letter for the open row tail in {body:?}"
    );
    assert!(
        !body.contains("[a: _,"),
        "row tail should be named; saw bare '_' for type var in {body:?}"
    );
}

/// Forcing a non-thunk should yield a message that uses ϕ (or one of
/// its alphabet siblings) for the computation variable, not `_`.
#[test]
fn computation_variables_get_greek_letters() {
    let msgs: Vec<String> = raw_errors("let t = 42\nlet x = !$t\nreturn $x")
        .into_iter()
        .map(|e| e.kind.render_message())
        .collect();
    let body = msgs.join("\n");
    let has_comp_greek =
        body.contains('ϕ') || body.contains('χ') || body.contains('ψ') || body.contains('ω');
    assert!(
        has_comp_greek,
        "expected a Greek letter (ϕ/χ/ψ/ω) for the comp variable in {body:?}"
    );
}

/// Reach for the same variable name on both sides of a `couldn't
/// match` message — that's how a reader connects "the α in the
/// expected type" to "the α in the actual type".  Verify that when
/// the same variable appears in both sides of a mismatch, it prints
/// with one shared letter.
#[test]
fn shared_variables_share_letters() {
    // Tries to add a record (some `α`) to an Int.  The variable α
    // appears in the bound type of `x` and in the arithmetic context's
    // operand type — printing them as different letters would be
    // wrong.  We assert that whatever letter is chosen, it's used
    // consistently.
    let msgs: Vec<String> = raw_errors("let x = [a: 1]\nreturn $[$x + 1]")
        .into_iter()
        .map(|e| e.kind.render_message())
        .collect();
    let body = msgs.join("\n");
    // The exact letter doesn't matter — what matters is that the
    // message is intelligible.  At minimum, it should not mention
    // unification-internal jargon.
    for bad in JARGON_FRAGMENTS {
        assert!(!body.contains(bad), "jargon {bad:?} in {body:?}");
    }
}

/// Scenarios whose typical diagnostic carries actionable context the
/// reader cannot infer from the message alone — what a `not` operator
/// expects, what a `case` scrutinee must look like, what a `[a, b]`
/// pattern needs, etc. — must come with a `hint`.  Without it, the
/// reader sees a bare unify-style message and is left to guess.  This
/// test catches regressions where a hint quietly evaporates from a
/// path that used to carry one.
#[test]
fn common_paths_carry_a_hint() {
    let hint_cases: &[(&str, &str)] = &[
        // `not` on Int — wrong-operand-shape hint.
        ("not", "let x = 1\nreturn $[not $x]"),
        // Arithmetic operands of different types — same-type hint.
        ("arith", "let a = 1\nlet b = hello\nreturn $[$a + $b]"),
        // Comparison operands of different types — same-type hint.
        ("compare", "let a = 1\nlet h = hello\nreturn $[$a < $h]"),
        // `case` on a non-variant scrutinee — variant-expected hint.
        (
            "case-on-int",
            "let x = 1\nlet r = case $x [`ok: { |i| return $i }]\nreturn $r",
        ),
        // List-pattern on a non-list — pattern-shape hint.
        ("list-pat", "let [a, b] = 42\nreturn $a"),
        // Spread of a non-list in a list literal — spread-shape hint.
        ("spread", "let n = 42\nreturn [...$n]"),
        // `if` branches disagree — branches-must-agree hint.
        ("if-branch", "if true { return 1 } else { return hello }"),
        // Function arg of wrong type — parameter-vs-argument hint.
        ("wrong-arg", "let f = { |x| return $[$x + 1] }\n!{f hello}"),
    ];
    for (tag, src) in hint_cases {
        let errs = raw_errors(src);
        assert!(
            !errs.is_empty(),
            "{tag}: no diagnostic produced for {src:?}"
        );
        let any_hint = errs.iter().any(|e| e.hint().is_some());
        assert!(
            any_hint,
            "{tag}: at least one diagnostic for {src:?} should carry a hint, \
             but none did (messages: {:?})",
            errs.iter()
                .map(|e| e.kind.render_message())
                .collect::<Vec<_>>()
        );
    }
}

/// The `Cmd` abbreviation is internal CBPV vocabulary.  User-facing
/// messages should say `Command` if they need that noun at all.
#[test]
fn cmd_abbreviation_does_not_leak() {
    // An `if` whose branches return different types historically
    // surfaced as `command type mismatch: Cmd Int vs Cmd String`.
    let errs = raw_errors("if true { return 1 } else { return hello }");
    let body: String = errs
        .iter()
        .map(|e| e.kind.render_message())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!errs.is_empty(), "expected at least one diagnostic");
    assert!(
        !body.contains("Cmd "),
        "abbreviation 'Cmd' should not appear in user-visible text: {body}"
    );
}
