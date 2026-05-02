//! Behavioural oracle for the HM static type checker.
//!
//! Every test parses + elaborates a small ral program and runs `typecheck()`.
//! The suite locks in current behaviour so that each refactor phase can be
//! verified green without modification.  If a phase breaks a test the
//! semantics have drifted; stop and investigate before continuing.

mod common;

use ral_core::typecheck::{CompTy, CompTyVar, Scheme, Ty, fmt_scheme};
use ral_core::{TypeError, elaborate, parse, typecheck};

fn raw_errors(src: &str) -> Vec<TypeError> {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    let comp = elaborate(&ast, Default::default());
    typecheck(&comp, common::prelude_schemes())
}

fn errors(src: &str) -> Vec<String> {
    raw_errors(src)
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

#[test]
fn fmt_scheme_shows_quantified_comp_vars() {
    let beta = CompTyVar(17);
    let scheme = Scheme {
        ty_vars: vec![],
        comp_ty_vars: vec![beta],
        mode_vars: vec![],
        row_vars: vec![],
        ty: Ty::Thunk(Box::new(CompTy::Var(beta))),
        comp_ty_bindings: vec![],
        cached_fv: None,
    };
    let rendered = fmt_scheme(&scheme);
    assert_eq!(rendered, "∀ϕ. ϕ");
}

#[test]
fn fmt_scheme_quantifies_cyclic_comp_roots() {
    let root = CompTyVar(29);
    let scheme = Scheme {
        ty_vars: vec![],
        comp_ty_vars: vec![],
        mode_vars: vec![],
        row_vars: vec![],
        ty: Ty::Thunk(Box::new(CompTy::Var(root))),
        comp_ty_bindings: vec![(root.0, CompTy::pure(Ty::Unit))],
        cached_fv: None,
    };
    let rendered = fmt_scheme(&scheme);
    assert_eq!(rendered, "∀ϕ. ϕ");
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
    // `_try` returns `[.ok: A | .err: ErrorRec]`.  Use `case` to
    // destructure; the .ok arm carries the body's value.
    ok("let r = _try { return 42 }; case $r [.ok: { |v| return $v }, .err: { |_| return 0 }]");
}

#[test]
fn builtin_try_err_field_types() {
    // The .err arm carries a typed error record with `status` etc.
    ok(
        "let r = _try { return 1 }; case $r \
         [.ok: { |_| return 0 }, .err: { |e| return $e[status] }]",
    );
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

// ─── Head-not-callable (T0011, surface phrasing) ──────────────────────────────

/// `'foo' bar baz` — a quoted string in command position with arguments.
/// The diagnostic must talk about the head being non-callable, not about
/// `Cmd a vs a → b` jargon nor about an argument-type mismatch.
#[test]
fn head_not_callable_string_with_args() {
    let errs = errors("'foo' bar baz");
    assert!(
        errs.iter()
            .any(|e| e.contains("cannot be used as a command head")),
        "expected 'cannot be used as a command head' message, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.contains("argument type")),
        "should not mention argument-type mismatch, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.contains("Cmd")),
        "should not surface internal `Cmd` jargon, got: {errs:?}"
    );
}

/// The error span must cover the whole command — head and args — so the
/// diagnostic underlines `'foo' bar baz`, not just the opening quote.
#[test]
fn head_not_callable_span_covers_whole_command() {
    let src = "'foo' bar baz";
    let errs = raw_errors(src);
    assert_eq!(
        errs.len(),
        1,
        "expected exactly one error, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    let pos = errs[0].pos.expect("error must carry a span");
    assert_eq!(
        (pos.start as usize, pos.end as usize),
        (0, src.len()),
        "span should cover the entire command `{src}`, got [{}, {})",
        pos.start,
        pos.end
    );
}

/// A bound non-callable value (`let x = 42; $x foo`) must trip the same
/// diagnostic — the value is data, not a function.
#[test]
fn head_not_callable_int_variable_with_args() {
    has_error("let x = 42\n$x foo", "cannot be used as a command head");
}

// ─── Variants and tag-keyed records (Phase A) ────────────────────────────────

#[test]
fn variant_construction_with_payload() {
    ok("let x = .ok 42\nreturn $x");
}

#[test]
fn variant_nullary() {
    ok("let x = .none\nreturn $x");
}

#[test]
fn variant_list_unifies_open_row() {
    // Each .ok / .err in a list extends the same open row.  The list is
    // homogeneous because the rows unify against a shared element type.
    ok("return [.ok 1, .err hello]");
}

#[test]
fn tag_keyed_record_literal() {
    ok("let r = [.dev: 8080, .prod: 443]\nreturn $r");
}

#[test]
fn variant_payload_type_mismatch() {
    // The payload must respect the variant's inferred type.  Re-using a
    // label with a different payload type forces a unification error.
    has_error(
        "let a = .ok 1\nlet b = .ok hello\nreturn [$a, $b]",
        "type mismatch",
    );
}

// ─── Case (sum eliminator, Phase B) ───────────────────────────────────────────

#[test]
fn case_exhaustive() {
    ok("let r = .ok 5\nlet x = case $r [.ok: { |x| return $x }, .err: { |_| return -1 }]\nreturn $x");
}

#[test]
fn case_open_scrutinee_absorbs_handler_labels() {
    // `.ok 5` produces an open variant `[.ok: Int | ρ]`.  A case with
    // .ok and .err arms forces ρ to extend with .err, leaving the
    // scrutinee row with both constructors after the case.
    ok("let r = .ok 5\nlet x = case $r [.ok: { |x| return $x }, .err: { |_| return -1 }]\nreturn $x");
}

#[test]
fn case_missing_arm_when_variant_has_more() {
    // The if branches force the variant row to include both .ok and
    // .err.  A case that handles only .ok leaves .err unhandled.
    has_error(
        "let r = if true { return .ok 1 } else { return .err hello }\nlet x = case $r [.ok: { |i| return $i }]\nreturn $x",
        "missing handlers",
    );
}

#[test]
fn case_handler_payload_mismatch() {
    // The .ok handler uses its payload as a String (via `upper`), but the
    // scrutinee's .ok was constructed with an Int payload — per-label
    // unification surfaces a type mismatch.
    has_error(
        "let r = .ok 5\nlet x = case $r [.ok: { |s| !{upper $s} }, .err: { |_| !{upper hello} }]\nreturn $x",
        "type mismatch",
    );
}

#[test]
fn case_arms_disagree_on_result() {
    // The two handlers return values of different types — the shared
    // result type cannot unify.
    has_error(
        "let r = .ok 5\nlet x = case $r [.ok: { |x| return $x }, .err: { |_| return hello }]\nreturn $x",
        "type mismatch",
    );
}

#[test]
fn case_scrutinee_must_be_variant_return() {
    // A function that cases on `!$x` must reject call sites where `x`
    // returns a non-variant value.
    has_error(
        "let bad = { |x| case !$x [.ok: { |v| return $v }] }\n\
         bad { return 1 }",
        "mismatch",
    );
}

// ─── Recursive computation types (Phase C) ────────────────────────────────────

#[test]
fn self_recursive_function() {
    // The canonical countdown.  The fix-point combinator binds `f` to
    // `Thunk(β)`; the body's recursive call unifies `β` with
    // `Fun(Int, β)`, producing a cyclic comp type which the unifier
    // accepts equi-recursively.
    ok("let f = { |n| if $[$n == 0] { return 0 } else { return !{f $[$n - 1]} } }\nreturn unit");
}

#[test]
fn recursive_stream_consumer() {
    // Pattern lifted from the streaming plan: a consumer that cases on a
    // forced thunk and recurses through the .more arm's payload.
    ok("let drain = { |s| case !$s [.more: { |p| !{drain $p[tail]} }, .done: { |_| return unit }] }\nreturn unit");
}

#[test]
fn recursive_stream_producer_typechecks() {
    // The canonical infinite-producer pattern.  Phase C's equi-recursive
    // comp types let the cycle Var(beta) ⟶ Fun(Int, F (Variant {.more:
    // {head: Int, tail: Thunk(beta)} | .done | ρ})) close in the
    // union-find without tripping an occurs check.
    ok("let nats = { |n| step-cons $n { !{nats $[$n + 1]} } }\nreturn unit");
}

#[test]
fn step_pipeline_rejects_non_recursive_tail() {
    // A `.more` node whose tail returns a non-Step variant would crash at
    // runtime on the second iteration; reject it statically at pipeline
    // boundaries.
    has_error(
        "return .more [head: 1, tail: { return .ok 2 }] | { |v| echo $v }",
        "invalid Step value in pipeline",
    );
}
