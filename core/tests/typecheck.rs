//! Behavioural oracle for the HM static type checker.
//!
//! Every test parses + elaborates a small ral program and runs `typecheck()`.
//! The suite locks in current behaviour so that each refactor phase can be
//! verified green without modification.  If a phase breaks a test the
//! semantics have drifted; stop and investigate before continuing.

mod common;

use ral_core::{elaborate, parse, typecheck};

fn errors(src: &str) -> Vec<String> {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    let comp = elaborate(&ast, Default::default());
    typecheck(&comp, common::prelude_schemes())
        .into_iter()
        .map(|e| e.kind.render_message())
        .collect()
}

fn ok(src: &str) {
    let errs = errors(src);
    assert!(
        errs.is_empty(),
        "expected no errors in {src:?}, got: {:?}",
        errs
    );
}

fn has_error(src: &str, fragment: &str) {
    let errs = errors(src);
    assert!(
        errs.iter().any(|e| e.contains(fragment)),
        "expected an error containing {fragment:?} in {src:?}, got: {:?}",
        errs
    );
}

// ─── Primitives ───────────────────────────────────────────────────────────────

#[test]
fn literal_int() {
    ok("return 42");
}

#[test]
fn literal_float() {
    ok("return 3.14");
}

#[test]
fn literal_str() {
    ok("return hello");
}

#[test]
fn literal_bool() {
    ok("return true");
    ok("return false");
}

#[test]
fn literal_unit() {
    ok("return unit");
}

// ─── Arithmetic ───────────────────────────────────────────────────────────────

#[test]
fn arith_int_add() {
    ok("return $[1 + 2]");
}

#[test]
fn arith_float_mul() {
    ok("return $[1.5 * 2.0]");
}

#[test]
fn arith_comparison_is_bool() {
    ok("return $[1 == 1]");
}

#[test]
fn arith_mixed_types_error() {
    // x is Str; using it in arithmetic with Int should unify Str with Int → mismatch.
    has_error("let x = hello; return $[$x + 1]", "mismatch");
}

// ─── Variables and let-binding ────────────────────────────────────────────────

#[test]
fn let_bind_and_use() {
    ok("let x = 42; return $x");
}

#[test]
fn unbound_variable_no_error() {
    // Unbound variable gets a fresh type; not a static error in ral.
    ok("return $undefined_var");
}

// ─── Lists ────────────────────────────────────────────────────────────────────

#[test]
fn list_homogeneous() {
    ok("return [1, 2, 3]");
}

#[test]
fn list_heterogeneous_error() {
    has_error("return [1, hello]", "mismatch");
}

#[test]
fn list_empty() {
    ok("return []");
}

#[test]
fn list_spread() {
    ok("let xs = [1, 2]; return [0, ...$xs]");
}

// ─── Maps and records ─────────────────────────────────────────────────────────

#[test]
fn map_literal_infers_as_record() {
    ok("let r = [foo: 1, bar: hello]; return $r[foo]");
}

#[test]
fn map_dynamic_key_is_homogeneous_map() {
    ok("let k = mykey; let m = [$k: 1]; return $m");
}

#[test]
fn map_empty_is_homogeneous() {
    ok("let m = [:]; return $m");
}

#[test]
fn map_spread_record() {
    ok("let a = [x: 1]; let b = [y: 2, ...$a]; return $b[x]");
}

#[test]
fn map_spread_fields_propagate() {
    // Under scoped-label semantics the spread source's fields are visible
    // in the result type.  Accessing a field from the spread must typecheck.
    ok("let base = [host: localhost, port: 80]; let r = [port: 9090, ...$base]; return $r[host]");
}

#[test]
fn map_spread_explicit_overrides_spread() {
    // The explicit field and the shadowed spread field both have type Int;
    // accessing port on the result must give Int regardless of source position.
    ok("let base = [port: 80, host: localhost]; let r = [...$base, port: 9090]; return $r[port]");
}

#[test]
fn map_spread_field_absent_from_closed_source_is_error() {
    // Spread source is a closed record that has no 'missing' field; accessing
    // 'missing' on the result must be a type error.
    has_error(
        "let base = [host: localhost]; let r = [...$base, port: 9090]; return $r[missing]",
        "field",
    );
}

#[test]
fn map_multiple_spreads_no_crash() {
    // Multiple spreads fall back to an imprecise open tail but must not crash.
    ok("let a = [x: 1]; let b = [y: 2]; let r = [...$a, ...$b, z: 3]; return $r[z]");
}

// ─── Pattern binding ──────────────────────────────────────────────────────────

#[test]
fn pattern_name() {
    ok("let x = 1; return $x");
}

#[test]
fn pattern_wildcard() {
    ok("let _ = 1; return unit");
}

#[test]
fn pattern_list_destructure() {
    ok("let [a, b] = [1, 2]; return $a");
}

#[test]
fn pattern_list_rest() {
    ok("let [head, ...tail] = [1, 2, 3]; return $head");
}

#[test]
fn pattern_map_destructure() {
    ok("let [x: v] = [x: 42]; return $v");
}

// ─── Let-generalization ───────────────────────────────────────────────────────

#[test]
fn let_generalize_polymorphic_identity() {
    // `id` should be ∀α. α → F α — usable at two different types.
    ok("let id = { |x| return $x }; let _ = !{id 1}; let _ = !{id hello}; return unit");
}

#[test]
fn let_generalize_list_id() {
    ok("let id = { |x| return $x }; let _ = !{id [1, 2]}; let _ = !{id [a, b]}; return unit");
}

// ─── Thunks and forcing ───────────────────────────────────────────────────────

#[test]
fn thunk_and_force() {
    ok("let t = { return 42 }; let x = !{t}; return $x");
}

#[test]
fn lambda_applied() {
    ok("let f = { |x| return $x }; let y = !{f hello}; return $y");
}

// ─── Record projection ────────────────────────────────────────────────────────

#[test]
fn record_field_access() {
    ok("let r = [a: 1, b: 2]; let _ = $r[a]; return unit");
}

#[test]
fn nested_record_access() {
    ok("let r = [x: [y: 42]]; let _ = $r[x][y]; return unit");
}

// ─── Recursive bindings are monomorphic ───────────────────────────────────────

#[test]
fn recursive_binding_no_error() {
    // A recursive function must type-check without generalising inside the rec group.
    ok(
        "let go = { |n| if $[$n == 0] { return unit } else { let _ = !{go $[$n - 1]}; return unit } }; return unit",
    );
}

// ─── Coercions (must NOT produce errors) ─────────────────────────────────────

#[test]
fn coercion_record_map_no_error() {
    // Record ↔ Map: pass a record literal to `keys` (expects [Str:α]).
    ok("let r = [a: 1, b: 2]; let _ = !{keys $r}; return unit");
}

// ─── Builtins ─────────────────────────────────────────────────────────────────

#[test]
fn builtin_if() {
    ok("if true { return 1 } else { return 2 }");
}

#[test]
fn builtin_if_branch_mismatch_error() {
    has_error("if true { return 1 } else { return hello }", "mismatch");
}

#[test]
fn builtin_map() {
    ok("_map { |x| return $[$x + 1] } [1, 2, 3]");
}

#[test]
fn builtin_filter() {
    ok("_filter { |x| return $[$x == 1] } [1, 2, 3]");
}

#[test]
fn builtin_try() {
    ok("let r = _try { return 42 }; let _ = $r[ok]; return unit");
}

#[test]
fn builtin_try_field_type() {
    ok("let r = _try { return 1 }; return $r[value]");
}

#[test]
fn builtin_glob() {
    ok("let xs = glob /tmp; return $xs");
}

#[test]
fn builtin_exists() {
    ok("let b = exists /tmp; return $b");
}

#[test]
fn builtin_len_on_list() {
    ok("let n = !{length [1, 2, 3]}; return $n");
}

#[test]
fn builtin_fork() {
    ok("let h = _fork { return 42 }; return $h");
}

#[test]
fn builtin_equal() {
    ok("let b = !{equal 1 1}; return $b");
}

#[test]
fn builtin_exit_without_status() {
    ok("exit");
}

// ─── Pipeline mode connections ────────────────────────────────────────────────

#[test]
fn pipeline_bytes_to_bytes_ok() {
    // Two external commands: both bytes mode.
    ok("echo foo | cat");
}

#[test]
fn pipeline_value_pass_through() {
    // A pure stage feeds its return value as implicit arg to next stage.
    ok("return hello | echo");
}

// ─── String interpolation ─────────────────────────────────────────────────────

#[test]
fn interpolation_no_error() {
    ok("let x = world; return \"hello $x\"");
}
