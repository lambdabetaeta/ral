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
    typecheck(
        &comp,
        ral_core::SessionSchemes::from_schemes(common::prelude_schemes()),
    )
    .err()
    .unwrap_or_default()
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
    has_error("let x = hello; return $[$x + 1]", "couldn't match");
}

// ─── Variables and let-binding ────────────────────────────────────────────────

#[test]
fn let_bind_and_use() {
    ok("let x = 42; return $x");
}

#[test]
fn crlf_after_numeric_let_preserves_integer_literal() {
    ok("let peek = 2\r\nreturn $[$peek + 1]");
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
    has_error("return [1, hello]", "couldn't match");
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

/// A dynamic map key whose static type isn't `String` is rejected at
/// typecheck time, lifting the runtime "map key must be a String" check
/// up the stack.  The rendered error is the generic Int-vs-String
/// mismatch; the map-specific hint travels alongside it.  (Bare numeric
/// keys like `[2: foo]` are caught even earlier — by the parser.)
#[test]
fn map_int_variable_key_is_typecheck_error() {
    let errs = raw_errors("let k = $[1]\nlet m = [$k: foo]\nreturn $m");
    assert!(
        errs.iter()
            .any(|e| e.kind.render_message().contains("Integer with type String")),
        "expected an Int-vs-String mismatch, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    let hint = errs
        .iter()
        .find_map(|e| e.hint.as_deref())
        .expect("expected a hint on the map-key mismatch");
    assert!(
        hint.contains("map keys must be Strings"),
        "hint should mention the map-key rule, got: {hint:?}"
    );
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
    // `id` should be polymorphic in its input/output type — usable at two different types.
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
        ty_bindings: vec![],
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
        ty_bindings: vec![],
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
    // Record ↔ Map: pass a record literal to `keys` (expects [Str:Value]).
    ok("let r = [a: 1, b: 2]; let _ = !{keys $r}; return unit");
}

// ─── Builtins ─────────────────────────────────────────────────────────────────

#[test]
fn builtin_if() {
    ok("if true { return 1 } else { return 2 }");
}

#[test]
fn builtin_if_branch_mismatch_error() {
    has_error(
        "if true { return 1 } else { return hello }",
        "couldn't match",
    );
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
    // `try body handler` returns body's value on success or handler's
    // value on failure.  Both branches must produce a compatible type.
    ok("let r = try { return 42 } { |_| return 0 }; return $r");
}

#[test]
fn builtin_try_err_field_types() {
    // The handler receives a typed error record with `status` etc.
    ok("let r = try { return 1 } { |e| return $e[status] }; return $r");
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

#[test]
fn background_pipeline_types_as_handle() {
    // `cmd &` suspends the pipeline; the binding is a Handle whose payload
    // is the pipeline's return type, recovered through `await`.
    ok("let h = echo hi &\nlet r = await $h\nreturn $r[value]");
}

#[test]
fn await_record_has_no_status_field() {
    // `await` returns `{value, stdout, stderr}`; the block's status is no
    // longer a field — projecting it must be a static error.
    has_error(
        "let h = !{spawn { return 1 }}\nlet r = await $h\nreturn $r[status]",
        "status",
    );
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

#[test]
fn pipeline_byte_into_value_stage_is_mode_mismatch() {
    // A byte-output stage feeding a value-input consumer is an adjacency
    // mismatch caught at type-check time (SPEC §20.4).  This exercises the
    // equality-strict `unify_mode`: the `ModeMismatch` arm (T0012) is live,
    // not dead code coerced away.  `echo` is byte-output; `length` reads a
    // value, not bytes.
    let errs = raw_errors("echo foo | length");
    assert!(
        errs.iter().any(|e| matches!(
            e.kind,
            ral_core::typecheck::TypeErrorKind::ModeMismatch { .. }
        )),
        "expected a ModeMismatch (T0012) for a byte→value adjacency, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
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

/// Nested unescaped `"` inside a `"..."` string silently splits the line
/// into [string, $deref, string], producing a head-not-callable error far
/// from the actual mistake.  The hint must point at the real cause —
/// nested double quotes — not the generic "wrap a callable" advice.
#[test]
fn head_not_callable_nested_double_quotes_hint() {
    let src = r#"let p = less
let m = "sh -c 'bat --pager="$(p)" --width=80'""#;
    let errs = raw_errors(src);
    let hint = errs
        .iter()
        .find_map(|e| e.hint.as_deref())
        .expect("expected a hint on the head-not-callable diagnostic");
    assert!(
        hint.contains("nested double quotes"),
        "hint should mention nested double quotes, got: {hint:?}"
    );
}

/// The generic `'foo' bar baz` mistake — bare-word args, not quoted —
/// must keep the original hint, not the nested-quote one.
#[test]
fn head_not_callable_bare_args_keeps_generic_hint() {
    let errs = raw_errors("'foo' bar baz");
    let hint = errs
        .iter()
        .find_map(|e| e.hint.as_deref())
        .expect("expected a hint on the head-not-callable diagnostic");
    assert!(
        hint.contains("function or a thunk"),
        "hint should be the generic head-not-callable advice, got: {hint:?}"
    );
}

/// F7: indexing a thunk-returning-record directly (`!$f[a]` parses as
/// `Force(Index($f, a))`, which indexes the thunk `$f`) must point at a
/// *followable* fix.  The old hint suggested `!$t[field]` — the exact
/// text that just failed.  The hint now names the working form
/// `!{!$t}[field]` (force `$f`, then index its result).
#[test]
fn index_on_thunk_hint_is_followable() {
    let errs = raw_errors("let f = { return [a: 1] }\necho !$f[a]");
    let hint = errs
        .iter()
        .find_map(|e| e.hint.as_deref())
        .expect("expected a hint on the index-on-thunk diagnostic");
    assert!(
        hint.contains("!{!$t}[field]"),
        "hint must name the followable form, got: {hint:?}"
    );
}

// ─── Variants and tag-keyed records (Phase A) ────────────────────────────────

#[test]
fn variant_construction_with_payload() {
    ok("let x = `ok 42\nreturn $x");
}

#[test]
fn variant_nullary() {
    ok("let x = `none\nreturn $x");
}

#[test]
fn variant_list_unifies_open_row() {
    // Each `ok / `err in a list extends the same open row.  The list is
    // homogeneous because the rows unify against a shared element type.
    ok("return [`ok 1, `err hello]");
}

#[test]
fn tag_keyed_record_literal() {
    ok("let r = [`dev: 8080, `prod: 443]\nreturn $r");
}

#[test]
fn variant_payload_type_mismatch() {
    // The payload must respect the variant's inferred type.  Re-using a
    // label with a different payload type forces a unification error.
    has_error(
        "let a = `ok 1\nlet b = `ok hello\nreturn [$a, $b]",
        "couldn't match",
    );
}

// ─── Case (sum eliminator, Phase B) ───────────────────────────────────────────

#[test]
fn case_exhaustive() {
    ok(
        "let r = `ok 5\nlet x = case $r [`ok: { |x| return $x }, `err: { |_| return -1 }]\nreturn $x",
    );
}

#[test]
fn case_open_scrutinee_absorbs_handler_labels() {
    // `` `ok 5 `` produces an open variant [`ok: Int | row].  A case with
    // `ok and `err arms forces the row to extend with `err, leaving the
    // scrutinee with both constructors after the case.
    ok(
        "let r = `ok 5\nlet x = case $r [`ok: { |x| return $x }, `err: { |_| return -1 }]\nreturn $x",
    );
}

#[test]
fn case_missing_arm_when_variant_has_more() {
    // The if branches force the variant row to include both `ok and
    // `err.  A case that handles only `ok leaves `err unhandled.
    has_error(
        "let r = if true { return `ok 1 } else { return `err hello }\nlet x = case $r [`ok: { |i| return $i }]\nreturn $x",
        "no handler for `err",
    );
}

#[test]
fn case_handler_payload_mismatch() {
    // The `ok handler uses its payload as a String (via `upper`), but the
    // scrutinee's `ok was constructed with an Int payload — per-label
    // unification surfaces a type mismatch.
    has_error(
        "let r = `ok 5\nlet x = case $r [`ok: { |s| !{upper $s} }, `err: { |_| !{upper hello} }]\nreturn $x",
        "couldn't match",
    );
}

#[test]
fn case_arms_disagree_on_result() {
    // The two handlers return values of different types — the shared
    // result type cannot unify.
    has_error(
        "let r = `ok 5\nlet x = case $r [`ok: { |x| return $x }, `err: { |_| return hello }]\nreturn $x",
        "couldn't match",
    );
}

#[test]
fn case_scrutinee_must_be_variant_return() {
    // A function that cases on `!$x` must reject call sites where `x`
    // returns a non-variant value.
    has_error(
        "let bad = { |x| case !$x [`ok: { |v| return $v }] }\n\
         bad { return 1 }",
        "couldn't match",
    );
}

// ─── Recursive computation types (Phase C) ────────────────────────────────────

#[test]
fn self_recursive_function() {
    // The canonical countdown.  The fix-point combinator binds `f` to
    // a thunk of a computation variable; the body's recursive call unifies that
    // with `Fun(Int, CompVar)`, producing a cyclic comp type which the unifier
    // accepts equi-recursively.
    ok("let f = { |n| if $[$n == 0] { return 0 } else { return !{f $[$n - 1]} } }\nreturn unit");
}

#[test]
fn recursive_stream_consumer() {
    // Pattern lifted from the streaming plan: a consumer that cases on a
    // forced thunk and recurses through the `more arm's payload.
    ok(
        "let drain = { |s| case !$s [`more: { |p| !{drain $p[tail]} }, `done: { |_| return unit }] }\nreturn unit",
    );
}

#[test]
fn recursive_stream_producer_typechecks() {
    // The canonical infinite-producer pattern.  Phase C's equi-recursive
    // comp types let the cycle CompVar ⟶ Fun(Int, F (Variant {`more:
    // {head: Int, tail: Thunk(CompVar)} | `done | row})) close in the
    // union-find without tripping an occurs check.
    ok("let nats = { |n| stream-cons $n { !{nats $[$n + 1]} } }\nreturn unit");
}

#[test]
fn stream_combinator_taking_value_unifies() {
    // A stream `map` written to take a `Stream` *value* (`case $s`, recursing
    // through `!$p[tail]`) is the same equi-recursive type as `from-lines`'
    // producer — but anchored at a ty-var rather than a comp-var.  Unifying
    // the two used to overflow the typechecker's stack; the one-sided
    // co-inductive obligations let it terminate and unify the two anchorings.
    ok(
        "let smap = { |f s| case $s [`more: { |p| stream-cons !{$f $p[head]} { !{smap $f !$p[tail]} } }, `done: { |_| stream-nil }] }\n\
         let s = !{ from-lines }\n\
         let mapped = !{ smap { |x| return $x } $s }\n\
         echo !{ stream-to-list $mapped }",
    );
}

#[test]
fn step_labelled_variant_pipes_as_plain_value() {
    // `x | f` is `f !{x}` at every value edge: a `more-labelled variant
    // is ordinary data, whatever its payload's shape, and the consumer
    // receives it whole.
    ok("return `more [head: 1, tail: { return `ok 2 }] | { |v| echo $v }");
}

#[test]
fn stream_piped_whole_into_element_consumer_is_static_error() {
    // The consumer receives the Step variant itself, not its elements;
    // a parameter constrained to Int clashes with the stream type, and
    // the hint points at the explicit eliminators.
    let errs = raw_errors(
        "let s = !{stream-cons 1 { !{stream-nil} }}\n\
         $s | { |e| return $[$e + 1] } | { |y| return $[$y * 10] }",
    );
    assert!(
        errs.iter().any(|e| e
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("lazy Step stream"))),
        "expected a stream-eliminator hint, got: {:?}",
        errs
    );
}

// ─── Control operators in value position ─────────────────────────────────────
//
// After the scope-base refactor, `within`, `try`, `guard`, `grant`, and
// `audit` are structural `CompKind` variants — no longer string-keyed
// builtins.  The parser keeps them reserved in bare-head and `let`-binding
// positions, but the user can still write `$try` (a variable reference to
// the name `try`).  That dereference would silently get a fresh type
// variable under the old `Val::Variable` arm; the typechecker now flags it.

#[test]
fn control_op_within_in_value_position_errors() {
    has_error(
        "let x = $within; return $x",
        "'within' is a control operator",
    );
}

#[test]
fn control_op_try_in_value_position_errors() {
    has_error("let x = $try; return $x", "'try' is a control operator");
}

#[test]
fn control_op_guard_in_value_position_errors() {
    has_error("let x = $guard; return $x", "'guard' is a control operator");
}

#[test]
fn control_op_grant_in_value_position_errors() {
    has_error("let x = $grant; return $x", "'grant' is a control operator");
}

// ─── grant policy shapes (TUTORIAL §16, SPEC §11.1) ──────────────────────────
//
// `exec`/`fs` entries carry a per-key policy whose value is a `String`
// (`'allow'`/`'deny'`), an empty/non-empty subcommand list, or — for `fs` — an
// inner read/write/deny map.  These are genuinely heterogeneous and key-shaped,
// inexpressible as one homogeneous element type, so the schema leaves them to
// the runtime decoder; each documented form must still pass `--check`.

#[test]
fn grant_exec_string_policy_typechecks() {
    ok(r#"grant [exec: [ls: "deny"]] { echo hi }"#);
}

#[test]
fn grant_exec_subpath_allow_typechecks() {
    ok(r#"grant [exec: ['/usr/bin/': 'allow']] { echo hi }"#);
}

#[test]
fn grant_exec_mixed_policy_shapes_typecheck() {
    // The TUTORIAL §16 example: a subcommand list, an empty list, and an
    // inline string policy in one map.
    ok(r#"grant [exec: [git: ['status', 'log'], make: [], '/usr/bin/': 'allow']] { echo hi }"#);
}

#[test]
fn grant_fs_read_write_typechecks() {
    ok(r#"grant [fs: [read: ['/home/project'], write: ['/tmp/build']], net: false] { echo hi }"#);
}

#[test]
fn grant_inner_value_errors_still_surface() {
    // The policy values stay runtime-dispatched, but their sub-expressions are
    // still inferred — a genuine type error inside a policy value is a static
    // error rather than being masked by the (now removed) outer schema clash.
    has_error(
        r#"grant [exec: [git: $[true + 1]]] { echo hi }"#,
        "couldn't match type Bool with type Integer",
    );
}

#[test]
fn control_op_audit_in_value_position_errors() {
    has_error("let x = $audit; return $x", "'audit' is a control operator");
}

// ─── within handler sorted bindings ──────────────────────────────────────────
//
// `infer_within` now recognises a literal `handlers:` map, infers each
// thunk under the handler calling convention, generalises the result
// computation, and binds it in a fresh handler frame for the body.

/// Handler installation for a builtin name is rejected. Builtin bindings are
/// language names, not handler names, so `length` cannot be rewritten by a
/// scoped handler.
#[test]
fn within_handler_for_builtin_is_rejected() {
    has_error(
        r#"within [handlers: [length: { |xs| "hi" }]] { let r = !{length [1, 2, 3]}; return $r }"#,
        "cannot install handler for builtin `length`",
    );
}

// ─── handler / alias mode preservation ───────────────────────────────────────
//
// A handler arm (or alias body) for an unknown head `h` defines `h`'s modes:
// its spec is fully fresh, so the arm pins it, while its value type stays free.
// Reinterpreting a known head pins the arm to that head's spec, and a clash
// there is a `ModeMismatch`.  The byte-channel discipline is enforced where
// pipeline channels connect, so a value-output head feeding a byte consumer is
// a `ModeMismatch` at the connection (`docs/SPEC.md` §4.2.1).

fn is_mode_mismatch(src: &str) -> bool {
    raw_errors(src).iter().any(|e| {
        matches!(
            e.kind,
            ral_core::typecheck::TypeErrorKind::ModeMismatch { .. }
        )
    })
}

/// A value-output `within` handler arm defines an unknown head's modes, so a
/// standalone use typechecks; the mismatch is a connection-point property,
/// surfacing only when the head's `∅` output feeds a byte consumer.
#[test]
fn within_handler_value_output_pipes_byte_consumer_is_mismatch() {
    assert!(
        is_mode_mismatch(r#"within [handlers: [foo: { return 3 }]] { foo | from-json }"#),
        "expected a ModeMismatch where the value-output handler arm feeds the from-json decoder"
    );
    ok(r#"within [handlers: [foo: { return 3 }]] { foo }"#);
}

/// A byte-output `within` handler arm defines the unknown head as
/// byte-output and typechecks.
#[test]
fn within_handler_byte_output_arm_ok() {
    ok(r#"within [handlers: [foo: { echo hi }]] { foo }"#);
}

/// A byte-output forwarding alias defines the unknown head as byte-output and
/// typechecks.
#[test]
fn alias_byte_output_forwarder_typechecks() {
    ok(r#"alias myecho { |a| /bin/echo ...$a }; myecho hi"#);
}

/// A value-output alias body defines the unknown head's modes, so the
/// definition typechecks on its own.
#[test]
fn alias_value_output_body_typechecks() {
    ok("alias foo { return 3 }\nreturn unit");
}

/// The value-output head's `∅` output is rejected where it feeds a byte
/// consumer: the alias binds for subsequent statements in the `Seq`, so
/// `foo | from-json` is a connection-point `ModeMismatch`.
#[test]
fn alias_value_output_piped_into_byte_consumer_is_mismatch() {
    assert!(
        is_mode_mismatch("alias foo { return 3 }\nfoo | from-json"),
        "expected a ModeMismatch where the value-output alias feeds the from-json decoder"
    );
}

/// An alias over an existing mode-preserving alias resolves the head's spec
/// from the prior alias's handler scheme and typechecks: byte output pins to
/// byte output.
#[test]
fn alias_over_mode_preserving_alias_typechecks() {
    ok(r#"alias one { echo one }; alias two { one }; two"#);
}

/// The catch-all `handler: { comp }` names no specific head, so the
/// mode-preservation rule does not constrain it — a value-output catch-all
/// arm is not rejected.
#[test]
fn catch_all_handler_arm_is_not_mode_pinned() {
    ok(r#"within [handler: { |n a| return 'x' }] { return unit }"#);
}

#[test]
fn local_binding_beats_handler() {
    ok(
        r#"within [handlers: [foo: { echo handler }]] { let foo = { return 41 }; let r = !{foo}; return $[$r + 1] }"#,
    );
}

#[test]
fn caret_skips_binding_but_not_handler() {
    has_error(
        r#"within [handlers: [cat: { return "mocked" }]] { let r = !{^cat nope}; return $[$r + 0] }"#,
        "couldn't match",
    );
}

/// Handler-body argument mismatch surfaces statically.
///
/// The handler `{ |x| x + 1 }` receives argv as a list, so arithmetic on
/// `$x` is ill-typed at installation time.
#[test]
fn within_handler_arity_mismatch_is_static_error() {
    has_error(
        r#"within [handlers: [foo: { |x| return $[$x + 1] }]] { foo "hello" }"#,
        "couldn't match",
    );
}

/// Non-literal handler value falls through gracefully.
///
/// The value `$h` is a `Val::Variable`, not a literal `Val::Thunk`, so
/// `collect_within_handler_bindings` produces no binding for `foo`.  The
/// body's call `foo 1` falls through to the builtin registry / external
/// path and typechecks without error (the name is unrecognised so a fresh
/// type is assigned).
#[test]
fn within_handler_non_literal_value_falls_through() {
    ok(r#"let h = { |x| return $[$x + 1] }; within [handlers: [foo: $h]] { foo 1 }"#);
}

/// Existing within env/dir fields still typecheck correctly after the refactor.
#[test]
fn within_env_and_dir_still_typecheck() {
    ok(r#"within [env: [KEY: "val"], dir: "/tmp"] { return "ok" }"#);
    // Passing an Int where a Map<String> is expected for `env:` — the
    // actual error message is "couldn't match" with Int vs Map.
    has_error(
        r#"within [env: 42, dir: "/tmp"] { return "ok" }"#,
        "couldn't match",
    );
}

// ─── alias handler bindings in Seq ───────────────────────────────────────────
//
// `infer_comp`'s `Seq` arm now pattern-matches alias statements at the IR shape
// `Exec(Bare("alias"), [String(n), Thunk(t)], [])` and binds `(n, generalised
// handler scheme)` for subsequent statements.  The Seq runs inside a type env
// frame so aliases do not leak past the Seq's scope.  Aliases inside
// conditionals / function bodies are NOT at Seq level and do not leak.
//
// Nullary aliases are typed as handler entries, not nullary CBPV functions:
// extra argv is inferred for local errors and then ignored by the static
// handler-call rule. The handler's return type is still pinned.

/// Nullary alias binds in TyEnv: `alias greet { return "hi" }; greet`
/// typechecks without error.  If the body returned an Int, using the result
/// in a String context (e.g. `str_concat "x" (greet)`) would be a type error;
/// here we verify the positive case and probe the inferred type by checking
/// that using the result in Int arithmetic IS a type error (because it's a
/// String, not an Int).
#[test]
fn alias_nullary_binds_in_tyenv() {
    // Positive: the seq typechecks.  The arm is byte-output (`echo`),
    // preserving the head's `F[μ, Bytes]` spec, while its value type stays
    // String — a handler reinterprets a head without retyping its modes.
    ok(r#"alias greet { echo hi; return "hi" }; greet"#);
    // Probe: the result is String, so using it in Int arithmetic is a type
    // error.  This would NOT error if the alias binding were absent (the
    // call would fall through to the external dispatcher and get a fresh
    // type variable).
    has_error(
        r#"let r = !{ alias greet { echo hi; return "hi" }; greet }; return $[$r + 0]"#,
        "couldn't match",
    );
    has_error(
        r#"let r = !{ alias greet { echo hi; return "hi" }; greet extra args }; return $[$r + 0]"#,
        "couldn't match",
    );
}

#[test]
fn value_lookup_does_not_reify_aliases_or_command_only_builtins() {
    has_error(
        r#"alias greet { return "hi" }; let f = $greet; return $f"#,
        "handler entry",
    );
    has_error("let f = $echo; return $f", "builtin command");
}

/// Alias lambda parameters receive the argv list, not one scalar command
/// argument per atom. This is an install-time error because arithmetic
/// constrains `$n` to `Integer`, while handler calling convention gives
/// it a list type.
#[test]
fn alias_parameter_receives_argv_list() {
    has_error(
        r#"alias add1 { |n| return $[$n + 1] }; add1 5"#,
        "couldn't match",
    );
}

/// An alias whose body constrains its argv element type rejects a call site
/// whose argument has the wrong type.  `$a[0]` is the first argv element;
/// `$[$a[0] + 1]` constrains the element to `Integer`, so `inc hello` (a
/// `String` argument) is a static error rather than a deferred runtime
/// failure.  The arm's `Fun` shape must reach the call site for this to fire.
#[test]
fn alias_argument_checked_against_arm_parameter() {
    has_error(
        r#"alias inc { |a| return $[$a[0] + 1] }; inc hello"#,
        "couldn't match",
    );
    // The same arm called with an Integer argument is well-typed.
    ok(r#"alias inc { |a| return $[$a[0] + 1] }; inc 5"#);
}

/// Last-pushed alias shadows earlier alias at typecheck: the second
/// `alias greet` re-binds in TyEnv (last-pushed wins).  The final `greet`
/// should see the second alias's Int return type.
#[test]
fn alias_last_pushed_shadows_earlier() {
    // Both aliases registered; the second one (returning Int) should win.
    // Positive: Int arithmetic on the second alias's return value is fine
    // (verifies the second binding actually wins and returns Int).
    ok(
        r#"alias greet { echo hi; return "hi" }; alias greet { echo 42; return 42 }; let r = !{greet}; return $[$r + 0]"#,
    );
    // Negative: if only the first alias (String) won, the Int arithmetic
    // would error.  Since the second wins (Int), arithmetic succeeds — and
    // using the result as if it were a List would be an error.
    // Use `return $[$r + 0]` vs `return $[$r + true]` (Bool vs Int mismatch)
    // to confirm the type is pinned to Int, not a free variable.
    has_error(
        r#"alias greet { echo hi; return "hi" }; alias greet { echo 42; return 42 }; let r = !{greet}; return $[$r + true]"#,
        "couldn't match",
    );
}

/// Alias inside a conditional does NOT leak to subsequent Seq statements.
/// The alias is inside an `if` branch, not at Seq level, so the follow-up
/// call `greet` does not see the TyEnv binding.  Assert: no panic; the
/// program compiles.  (The call falls through to the external dispatcher
/// and gets a fresh type.)
#[test]
fn alias_inside_conditional_does_not_leak() {
    // The alias is inside the `then` branch — not at the enclosing Seq level.
    // The subsequent `greet` should NOT see the binding.  Compilation must
    // not panic; the type of `greet` is left free (external dispatch).
    ok("if true { alias greet { return \"hi\" } } else { return unit }; greet");
}

/// Multi-statement Seq: bindings from an alias are visible to all subsequent
/// statements, including an intermediate `let` and the final expression.
#[test]
fn alias_binding_visible_to_all_subsequent_statements() {
    // `r` is bound to the result of `greet` (String); the final `greet` also
    // sees the binding.  Both should typecheck without error.
    ok(r#"alias greet { echo hi; return "hi" }; let r = !{greet}; greet"#);
    // Verify `r` really has String type by checking arithmetic on it errors.
    has_error(
        r#"let _ = !{ alias greet { echo hi; return "hi" }; let r = !{greet}; return $[$r + 0] }; return unit"#,
        "couldn't match",
    );
}

/// The typechecker recognises alias/unalias by IR shape.  If the
/// elaborator changes the shape and the typechecker doesn't see it,
/// aliases silently fall through to external exec — wrong behaviour
/// with no error.  This test round-trips the canonical shape through
/// parse → elaborate → typecheck to make the coupling explicit.
#[test]
fn alias_ir_shape_round_trips() {
    // Canonical alias shape: recognised and bound.
    ok("alias g { echo 42; return 42 }; return $[!{g} + 1]");

    // Unalias recognised.
    ok("alias g { echo 42; return 42 }; unalias g; g");

    // Spread in alias position is a static error (would silently
    // fall through to external exec without the explicit check).
    has_error(
        "let xs = [\"g\"]; alias ...xs { return 42 }",
        "malformed alias",
    );

    // Wrong arg count for unalias in a Seq is a static error.
    has_error("unalias g; unalias", "malformed unalias");
}

#[test]
fn unalias_removes_only_static_alias_binding() {
    has_error(
        r#"alias greet { echo 41; return 41 }; unalias greet; let r = !{greet}; return $[$r + 0]"#,
        "couldn't match",
    );
    ok(
        r#"within [handlers: [greet: { echo 41; return 41 }]] { unalias greet; let r = !{greet}; return $[$r + 1] }"#,
    );
}

#[test]
fn builtin_command_signatures_are_explicit() {
    ok("range 1 3");
    has_error(r#"range "a" 3"#, "couldn't match");
    ok("from-lines");
    ok("from-lines \"a\\nb\\n\"");
    ok("_type 42");
    has_error("fail [status: 0]", "fail requires a nonzero status");
}

#[test]
fn type_probe_threads_argument_type() {
    // `_type` is `α → F α`: the argument's type flows through to the
    // result, so a String probed by `_type` is still rejected in
    // arithmetic. A fresh, decoupled result type would mask this.
    ok(r#"let x = !{_type 41}; return $[$x + 1]"#);
    has_error(
        r#"let x = !{_type hello}; return $[$x + 1]"#,
        "couldn't match",
    );
}

// ─── IR mode annotation ───────────────────────────────────────────────────────
//
// The annotation pass writes the checker's ground mode verdicts into a
// rebuilt IR: a `Wire` per pipeline stage, and the RHS output `ByteMode`
// on every `Bind`.  Schemes stay on the top-level spine only; wires and
// RHS modes go everywhere, at any depth.

use ral_core::ir::{Comp, CompKind, IrPattern};
use ral_core::mode::{ByteMode, Wire};

/// Compile `src` to an annotated comp, asserting it type-checks.
fn annotated(src: &str) -> Comp {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    let comp = elaborate(&ast, Default::default());
    typecheck(
        &comp,
        ral_core::SessionSchemes::from_schemes(common::prelude_schemes()),
    )
    .unwrap_or_else(|errs| panic!("expected no errors in {src:?}, got: {errs:?}"))
}

/// Every `Pipeline` node's `wires` slot, reachable anywhere in the tree.
fn all_pipeline_wires(comp: &Comp) -> Vec<(usize, Vec<Wire>)> {
    let mut out = Vec::new();
    common::walk_comp(comp, &mut |c| {
        if let CompKind::Pipeline { stages, wires } = &c.item {
            out.push((stages.len(), wires.clone()));
        }
    });
    out
}

#[test]
fn top_level_pipeline_carries_ground_wires() {
    let comp = annotated(r#"/bin/echo hi | /bin/cat"#);
    let pipelines = all_pipeline_wires(&comp);
    assert_eq!(pipelines.len(), 1, "expected exactly one pipeline node");
    let (stage_count, wires) = &pipelines[0];
    assert_eq!(*stage_count, 2, "two-stage pipeline");
    assert_eq!(wires.len(), *stage_count, "one wire per stage");
    // `/bin/echo` emits bytes with an open input (defaulted to `Empty`);
    // the adjacency unifies `/bin/cat`'s input to `Bytes`.
    assert_eq!(
        wires[0],
        Wire {
            input: ByteMode::Empty,
            output: ByteMode::Bytes
        }
    );
    assert_eq!(
        wires[1],
        Wire {
            input: ByteMode::Bytes,
            output: ByteMode::Bytes
        }
    );
}

#[test]
fn pipeline_inside_thunk_body_carries_wires() {
    // The pipeline lives in a lambda body under a `let`; the pass must
    // descend past the spine to reach it.
    let comp = annotated(r#"let f = { |x| /bin/echo $x | /bin/cat }"#);
    let pipelines = all_pipeline_wires(&comp);
    assert_eq!(
        pipelines.len(),
        1,
        "expected one pipeline nested in the lambda body"
    );
    let (stage_count, wires) = &pipelines[0];
    assert_eq!(*stage_count, 2);
    assert_eq!(wires.len(), 2);
}

#[test]
fn bind_rhs_output_mode_is_ground() {
    // A byte-producing RHS (`echo`, `F[μ, Bytes]`) records `Bytes`; a
    // pure value RHS (`return 42`, `F[∅, ∅]`) records `Empty`.
    let comp = annotated(r#"let x = echo hi; let y = return 42; return unit"#);
    let mut binds = Vec::new();
    common::walk_comp(&comp, &mut |c| {
        if let CompKind::Bind {
            pattern: IrPattern::Name(name),
            rhs_output,
            ..
        } = &c.item
        {
            binds.push((name.clone(), *rhs_output));
        }
    });
    assert_eq!(
        binds,
        vec![
            ("x".to_string(), ByteMode::Bytes),
            ("y".to_string(), ByteMode::Empty),
        ]
    );
}

/// The single ground output mode written onto the `x` bind of `src`.
fn bind_x_output(src: &str) -> ByteMode {
    let comp = annotated(src);
    let mut found = None;
    common::walk_comp(&comp, &mut |c| {
        if let CompKind::Bind {
            pattern: IrPattern::Name(name),
            rhs_output,
            ..
        } = &c.item
            && name == "x"
        {
            found = Some(*rhs_output);
        }
    });
    found.expect("a bind named `x`")
}

#[test]
fn if_byte_branches_join_to_byte_output() {
    // Both arms emit bytes, so the conditional's output channel is `Bytes`.
    assert_eq!(
        bind_x_output("let x = if true { echo a } else { echo b }; return unit"),
        ByteMode::Bytes
    );
}

#[test]
fn if_value_branches_join_to_value_output() {
    // Both arms are pure values, so the conditional carries no byte output.
    assert_eq!(
        bind_x_output("let x = if true { return 1 } else { return 2 }; return unit"),
        ByteMode::Empty
    );
}

#[test]
fn nested_binds_carry_no_scheme_while_spine_does() {
    // The spine `let g` carries a scheme; a `let inner` under the lambda
    // body evaluates in a block scope and never installs, so it stays
    // `scheme: None`.
    let comp = annotated(r#"let g = { |x| let inner = return $x; return $inner }; return unit"#);
    let mut spine_named = None;
    let mut nested_named = None;
    common::walk_comp(&comp, &mut |c| {
        if let CompKind::Bind {
            pattern: IrPattern::Name(name),
            scheme,
            ..
        } = &c.item
        {
            match name.as_str() {
                "g" => spine_named = Some(scheme.is_some()),
                "inner" => nested_named = Some(scheme.is_some()),
                _ => {}
            }
        }
    });
    assert_eq!(spine_named, Some(true), "spine bind carries a scheme");
    assert_eq!(nested_named, Some(false), "nested bind carries no scheme");
}

// ─── Row termination and duplicate-key semantics ──────────────────────────────
//
// These pin the three row-subsystem repairs from dev/docs/260611_deep-review.md
// (T1, T2, T3).  Before the fix, the first two overflowed the host stack during
// `--check`; the third disagreed with the runtime on duplicate keys.

/// T1.  Both branches spread the *same* parameter row, so `merge_branches`
/// unifies `{x: Int | ρ}` against `{y: Int | ρ}` over one shared tail ρ.  The
/// Rémy rewrite has no finite or rational solution for mismatched heads over a
/// shared tail; the unifier must report the infinite-row error rather than
/// re-entering with a fresh tail forever.
#[test]
fn row_shared_tail_mismatched_heads_is_recursive_row_error() {
    has_error(
        "let c = true\n\
         let f = { |r| if $c { return [x: 1, ...$r] } else { return [y: 2, ...$r] } }\n\
         return $f",
        "infinite row",
    );
}

/// T2.  The else branch builds a record whose field type is itself a record
/// spreading the parameter row, so unification would install a row binding
/// `ρ = {x: {n: Int, ...ρ}}` — a cycle that passes *through a field type*, not
/// the spine.  The occurs check descends into field types and rejects it.
#[test]
fn row_cycle_through_field_type_is_recursive_row_error() {
    has_error(
        "let c = true\n\
         let f = { |r| if $c { return $r } else { return [x: [n: 1, ...$r]] } }\n\
         return $f",
        "infinite row",
    );
}

/// T3.  A duplicate explicit key resolves last-wins, matching the runtime
/// `Value::map`.  Here the last `x` is a String, so reading `$m[x]` in Int
/// arithmetic is a static type error — the same shape the runtime rejects.
#[test]
fn duplicate_key_last_wins_string_then_arith_is_error() {
    has_error(
        "let m = [x: 1, x: \"two\"]\nlet y = $[$m[x] + 1]\nreturn $y",
        "couldn't match",
    );
}

/// T3, the complementary direction: the last `x` is an Int, so reading it in
/// Int arithmetic typechecks.  Under the old first-wins checker this was a
/// String-vs-Int error; last-wins makes it well-typed, consistent with the
/// runtime which would compute `2`.
#[test]
fn duplicate_key_last_wins_int_then_arith_ok() {
    ok("let m = [x: \"two\", x: 1]\nreturn $[$m[x] + 1]");
}

/// T3 for `case`: a duplicated tag arm resolves to the last arm.  The first
/// `` `ok `` arm would treat the payload as a String (via `upper`), but the
/// last arm uses it as the Int the scrutinee carries; last-wins keeps the Int
/// arm, so the program typechecks.
#[test]
fn duplicate_case_arm_last_wins() {
    ok("let r = `ok 5\n\
         let x = case $r [`ok: { |s| !{upper $s} }, `ok: { |i| return $[$i + 1] }, `err: { |_| return 0 }]\n\
         return $x");
}

// ─── LetRec slot inference (T7) ───────────────────────────────────────────────
//
// `LetRec { slot: Some(i) }` re-establishes a mutually-recursive group in a
// throwaway scope and yields binding `i`'s lambda.  These nodes are synthesised
// by the evaluator at runtime, never by elaboration, so they are built here
// directly.  The arm used to return an unconstrained fresh type without
// inferring the group's bodies, so a body type error never surfaced; it now
// infers the group and returns binding `i`'s thunk type.

use ral_core::source::Spanned;
use std::sync::Arc;

/// A one-binding letrec group `letrec { only = { |x| <body> } } in slot 0`,
/// built as the `slot: Some(0)` IR the evaluator would synthesise.
fn letrec_slot0(body: CompKind) -> Comp {
    let lam = Spanned::synthetic(CompKind::Lam {
        param: IrPattern::Name("x".into()),
        body: Arc::new(Spanned::synthetic(body)),
    });
    let bindings = Arc::new(vec![(
        "only".to_string(),
        ral_core::Val::Thunk(Arc::new(lam)),
    )]);
    Spanned::synthetic(CompKind::LetRec {
        slot: Some(0),
        bindings,
    })
}

fn typecheck_comp(comp: &Comp) -> Vec<String> {
    typecheck(
        comp,
        ral_core::SessionSchemes::from_schemes(common::prelude_schemes()),
    )
    .err()
    .unwrap_or_default()
    .into_iter()
    .map(|e| e.kind.render_message())
    .collect()
}

/// A type error inside a `slot: Some` group's body surfaces: the lambda body
/// forces an `Int`, which is not a thunk.
#[test]
fn letrec_slot_body_type_error_surfaces() {
    let comp = letrec_slot0(CompKind::Force(ral_core::Val::Int(1)));
    let errs = typecheck_comp(&comp);
    assert!(
        !errs.is_empty(),
        "expected a body type error from the slot group, got none"
    );
}

/// A well-typed `slot: Some` group typechecks: the lambda body returns its
/// parameter.
#[test]
fn letrec_slot_well_typed_group_ok() {
    let comp = letrec_slot0(CompKind::Return(ral_core::Val::Variable("x".into())));
    let errs = typecheck_comp(&comp);
    assert!(
        errs.is_empty(),
        "expected the slot group to typecheck, got: {errs:?}"
    );
}
