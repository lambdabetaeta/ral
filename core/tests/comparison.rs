//! Regression tests for the comparison family (rec. A9; findings B1, B2,
//! E6, B5).  One total structural `equal`, one total ordering backing
//! `lt`/`gt`/`sort-list`/`sort-list-by` and the `$[…]` ordering operators.
//!
//! The harness mirrors `scope_escapes.rs`: bootstrap a prelude-registered
//! `Shell`, then drive each source string through `eval_top_level` like a
//! REPL turn.  The defects below all passed `--check` and the prior suite,
//! so each test is a coverage gap the doc facet let drift.

mod common;

use ral_core::evaluator;
use ral_core::types::{Break, Shell, Value};
use ral_core::{Comp, CompileOutcome, builtins, compile_and_typecheck};

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

fn compile_against(shell: &Shell, source: &str) -> std::sync::Arc<Comp> {
    match compile_and_typecheck(source, shell.session_schemes()) {
        CompileOutcome::Compiled(c) => std::sync::Arc::new(c),
        CompileOutcome::Parse(e) => panic!("parse: {source:?}: {e}"),
        CompileOutcome::Types(errs) => {
            let msgs: Vec<_> = errs.iter().map(|e| e.kind.render_message()).collect();
            panic!("type: {source:?}: {}", msgs.join("; "));
        }
    }
}

fn eval(shell: &mut Shell, source: &str) -> ral_core::types::Settled<Value> {
    let comp = compile_against(shell, source);
    evaluator::eval_top_level(&comp, shell)
}

/// Run `source`, expecting it to return `Value::Bool(expected)`.
fn expect_bool(source: &str, expected: bool) {
    let mut shell = fresh_shell();
    match eval(&mut shell, source) {
        Ok(Value::Bool(b)) => assert_eq!(b, expected, "{source:?}"),
        Ok(other) => panic!("{source:?}: expected Bool({expected}), got {other:?}"),
        Err(e) => panic!("{source:?}: expected Bool({expected}), got error {e:?}"),
    }
}

/// Run `source`, expecting a `Break::Error` whose message contains `needle`.
fn expect_error(source: &str, needle: &str) {
    let mut shell = fresh_shell();
    match eval(&mut shell, source) {
        Err(Break::Error(e)) => assert!(
            e.message.contains(needle),
            "{source:?}: error {:?} should mention {needle:?}",
            e.message
        ),
        Ok(v) => panic!("{source:?}: expected an error mentioning {needle:?}, got {v:?}"),
        Err(other) => panic!("{source:?}: expected Break::Error, got {other:?}"),
    }
}

// ── B1 — `equal` is total and reflexive ──────────────────────────────────

#[test]
fn equal_bytes_is_reflexive() {
    // Pre-fix: `values_equal` had no Bytes arm, so `_ => false` made a Bytes
    // value unequal to itself.
    expect_bool("let b = !{to-bytes [65, 66]}; return !{equal $b $b}", true);
    expect_bool(
        "return !{equal !{to-bytes [1, 2]} !{to-bytes [1, 3]}}",
        false,
    );
}

#[test]
fn equal_variants_compare_structurally() {
    // Optionality-via-variants: `ok`/`err` and payloaded constructors must
    // compare by label and payload, not fall to `_ => false`.  Bare-word
    // variants greedily absorb a following atom as payload, so each operand
    // is bound first to keep the two arguments separate.
    expect_bool("let a = `ok; let b = `ok; return !{equal $a $b}", true);
    expect_bool("let a = `ok; let b = `err; return !{equal $a $b}", false);
    expect_bool(
        "let a = `some 1; let b = `some 1; return !{equal $a $b}",
        true,
    );
    expect_bool(
        "let a = `some 1; let b = `some 2; return !{equal $a $b}",
        false,
    );
    expect_bool(
        "let a = `some 1; let b = `none; return !{equal $a $b}",
        false,
    );
}

#[test]
fn equal_on_computation_values_errors() {
    // Lambda/Block/Handle are an undeliverable comparison type: an error,
    // not a silent `false` (which would also have made them un-reflexive).
    expect_error("equal { return 1 } { return 1 }", "cannot compare");
    expect_error("let f = { |x| return $x }; equal $f $f", "cannot compare");
}

#[test]
fn equal_nested_collection_with_block_errors() {
    // The undeliverable judgment propagates through structural recursion.
    expect_error("equal [{ return 1 }] [{ return 1 }]", "cannot compare");
}

// ── B2 / E6 — one total ordering ─────────────────────────────────────────

#[test]
fn lt_gt_are_numeric_on_ints() {
    // Pre-fix: `str_cmp` compared `to_string()`, so "10" < "2".
    expect_bool("return !{lt 2 10}", true);
    expect_bool("return !{gt 2 10}", false);
    expect_bool("return !{lt 10 2}", false);
}

#[test]
fn lt_gt_lift_mixed_int_float() {
    expect_bool("return !{lt 2.5 10}", true);
    expect_bool("return !{gt 2.5 10}", false);
}

#[test]
fn lt_gt_are_lexicographic_on_strings() {
    expect_bool("return !{lt abc def}", true);
    expect_bool("return !{gt xyz abc}", true);
}

#[test]
fn ordering_on_uncomparable_pairs_errors() {
    expect_error("lt 5 hello", "cannot compare");
    expect_error("gt 5 hello", "cannot compare");
}

#[test]
fn expression_ordering_keeps_i64_precision() {
    // E6: `$[a > b]` routed both operands through f64 and lost precision
    // above 2^53.  Now it shares `value_ordering`, comparing Int·Int as i64.
    expect_bool("return $[9007199254740993 > 9007199254740992]", true);
    expect_bool("return $[9007199254740992 < 9007199254740993]", true);
}

// ── B2 — sort-list / sort-list-by sort by value, not by `to_string` ──────

#[test]
fn sort_list_is_ascending_numeric() {
    let mut shell = fresh_shell();
    let v = eval(&mut shell, "return !{sort-list [10, 9, 2]}").expect("sort-list");
    let expected = Value::list(vec![Value::Int(2), Value::Int(9), Value::Int(10)]);
    assert_eq!(v, expected);
}

#[test]
fn sort_list_by_uses_key_ordering() {
    let mut shell = fresh_shell();
    let v = eval(
        &mut shell,
        "return !{sort-list-by { |x| return $[0 - $x] } [2, 9, 10]}",
    )
    .expect("sort-list-by");
    let expected = Value::list(vec![Value::Int(10), Value::Int(9), Value::Int(2)]);
    assert_eq!(v, expected);
}

// ── B5 — arity-1 string builtins reject zero arguments ───────────────────

#[test]
fn arity_one_string_builtins_require_their_argument() {
    // Pre-fix: `arg0_str` used `unwrap_or_default()`, so `upper`/`lower`/etc.
    // silently operated on "" with no argument, contradicting fixed arity.
    expect_error("upper", "upper requires 1 argument");
    expect_error("lower", "lower requires 1 argument");
    expect_error("dedent", "dedent requires 1 argument");
    expect_error("shell-quote", "shell-quote requires 1 argument");
    expect_error("shell-split", "shell-split requires 1 argument");
}
