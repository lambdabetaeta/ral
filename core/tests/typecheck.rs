//! Behavioural oracle for the HM static type checker.
//!
//! Every test parses + elaborates a small ral program and runs `typecheck()`.
//! The suite locks in current behaviour so that each refactor phase can be
//! verified green without modification.  If a phase breaks a test the
//! semantics have drifted; stop and investigate before continuing.

mod common;

use ral_core::typecheck::{CompTy, CompTyVar, Scheme, Ty, fmt_scheme};
use ral_core::{TypeError, elaborator::elaborate, syntax::parser::parse, typecheck};

fn raw_errors(src: &str) -> Vec<TypeError> {
    errors_against(src, &ral_core::HostSurface::default())
}

/// `src` as a session dressed with `surface` sees it.  Almost everything is
/// checked against the bare core table `raw_errors` passes; a name only a host
/// installs must be checked against a surface that carries it, or the checker
/// reads it as an external and answers about something else.
fn errors_against(src: &str, surface: &ral_core::HostSurface) -> Vec<TypeError> {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    let comp = elaborate(&ast, std::collections::HashSet::default(), "")
        .unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
    typecheck(
        &comp,
        ral_core::SessionSchemes::from_schemes(common::prelude_schemes(), surface.builtin_table()),
    )
    .err()
    .unwrap_or_default()
}

/// A surface holding `detach`, the base frame core publishes but does not
/// install: a host takes it together with the birth budget it spends.
#[cfg(unix)]
fn detach_surface() -> ral_core::HostSurface {
    ral_core::HostSurface {
        statics: vec![ral_core::builtins::DETACH_BUILTIN],
        ..Default::default()
    }
}

/// Whether some error's guidance sentence contains `fragment` — the suite's way
/// of asking which vocabulary a diagnosis was phrased in.
fn has_hint(errs: &[TypeError], fragment: &str) -> bool {
    errs.iter()
        .any(|e| e.hint().is_some_and(|h| h.contains(fragment)))
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
        "expected no errors in {src:?}, got: {errs:?}"
    );
}

/// [`ok`], against a session dressed with `surface`.
#[cfg(unix)]
fn ok_against(src: &str, surface: &ral_core::HostSurface) {
    let errs: Vec<_> = errors_against(src, surface)
        .into_iter()
        .map(|e| e.kind.render_message())
        .collect();
    assert!(
        errs.is_empty(),
        "expected no errors in {src:?}, got: {errs:?}"
    );
}

fn has_error(src: &str, fragment: &str) {
    let errs = errors(src);
    assert!(
        errs.iter().any(|e| e.contains(fragment)),
        "expected an error containing {fragment:?} in {src:?}, got: {errs:?}"
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
    ok("return ()");
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
        .find_map(ral_core::TypeError::hint)
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
fn pattern_wildcard() {
    ok("let _ = 1; return ()");
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
    ok("let id = { |x| return $x }; let _ = !{id 1}; let _ = !{id hello}; return ()");
}

#[test]
fn let_generalize_list_id() {
    ok("let id = { |x| return $x }; let _ = !{id [1, 2]}; let _ = !{id [a, b]}; return ()");
}

#[test]
fn let_generalize_through_list_pattern() {
    ok("let [f, g] = [{ |x| return $x }, { |y| return $y }]; \
        let _ = !{f 1}; let _ = !{f hello}; let _ = !{g 2}; return ()");
}

#[test]
fn let_generalize_through_map_pattern() {
    ok("let [id: f] = [id: { |x| return $x }]; \
        let _ = !{f 1}; let _ = !{f hello}; return ()");
}

#[test]
fn fmt_scheme_shows_quantified_comp_vars() {
    let beta = CompTyVar(17);
    let scheme = Scheme {
        ty_vars: vec![],
        comp_ty_vars: vec![beta],
        route_vars: vec![],
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
        route_vars: vec![],
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
    ok("let r = [a: 1, b: 2]; let _ = $r[a]; return ()");
}

#[test]
fn nested_record_access() {
    ok("let r = [x: [y: 42]]; let _ = $r[x][y]; return ()");
}

// ─── Recursive bindings are monomorphic ───────────────────────────────────────

#[test]
fn recursive_binding_no_error() {
    // A recursive function must type-check without generalising inside the rec group.
    ok(
        "let go = { |n| if $[$n == 0] { return () } else { let _ = !{go $[$n - 1]}; return () } }; return ()",
    );
}

// ─── Coercions (must NOT produce errors) ─────────────────────────────────────

#[test]
fn coercion_record_map_no_error() {
    // Record ↔ Map: pass a record literal to `keys` (expects [Str:Value]).
    ok("let r = [a: 1, b: 2]; let _ = !{keys $r}; return ()");
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
fn chain_arms_agree_on_result() {
    ok("let zzv = return 1 ? return 2; return $[$zzv + 1]");
}

#[test]
fn builtin_map() {
    ok("map { |x| return $[$x + 1] } [1, 2, 3]");
}

#[test]
fn builtin_filter() {
    ok("filter { |x| return $[$x == 1] } [1, 2, 3]");
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

// ─── guard / audit scopes ─────────────────────────────────────────────────────
//
// `guard` passes its body's payload route and value through — cleanup runs
// for its effects alone, its writes escaping like any discarded statement's —
// while `audit` is fixed `Value`, returning its record whatever the body did.

#[test]
fn guard_passes_its_bodys_byte_route_through() {
    ok("guard { echo hi } { return () } | from-string");
}

#[test]
fn guard_cleanup_bytes_are_ambient_not_a_payload() {
    // Cleanup chatter escapes to the ambient stream — inside a stage, that
    // stream is the pipe.  Neither the guard's payload route nor its
    // cleanup's decides whether the edge to `cat` is allowed: it is
    // positional, and no route takes part in it.
    ok("guard { return () } { echo hi } | cat");
}

#[test]
fn guard_input_joins_bytes_dominant_when_either_always_arm_reads() {
    // Both arms always run, so input joins bytes-dominant rather than
    // alternating: an upstream byte producer satisfies the guard even
    // though only one of the two arms actually reads it.
    ok("echo hi | guard { from-string } { return () }");
    ok("echo hi | guard { return () } { from-string }");
}

#[test]
fn audit_consumes_piped_bytes() {
    ok("echo hi | audit { from-string }");
}

#[test]
fn audit_record_value_field_is_body_raw_value_not_invented() {
    // The record's `value` field is `infer_audit`'s `alpha` — the body's
    // own raw value type, unified with nothing else — so it tracks
    // whatever the body actually returns rather than a synthesized String.
    ok("let r = audit { return 42 }; return $[$r[value] + 1]");
    // `echo`'s raw return is `Unit`, not `String`: if the record's value
    // field invented a decoded String observation, this would typecheck.
    has_error(
        "let r = audit { echo hi }; return $[$r[value] + 1]",
        "couldn't match",
    );
}

/// Pins: `v` observes the record `audit` returns — its payload — not the
/// piped `String`.  This test asserted the opposite until WF-1 was repealed,
/// and the reversal is the repeal's point: an audit-tailed pipeline had
/// `output = Bytes`, the runtime read that field as "the payload is bytes",
/// and threw the record away.  `cat`'s bytes still reach the pipeline's
/// stdout; they are simply not what `v` binds.
#[test]
fn pipeline_ending_in_audit_binds_the_audit_record() {
    let comp = annotated("let v = echo a | audit { cat }; return ()");
    let mut bound = None;
    common::walk_comp(&comp, &mut |c| {
        if let CompKind::Bind {
            pattern,
            scheme: Some(scheme),
            ..
        } = &c.item
            && let IrPattern::Name(name) = pattern.as_ref()
            && name == "v"
        {
            bound = Some(scheme.ty.clone());
        }
    });
    let Some(Ty::Record(row)) = bound else {
        panic!("`v` must bind `audit`'s record, got: {bound:?}");
    };
    let mut labels = Vec::new();
    let mut cur = &row;
    while let ral_core::typecheck::Row::Extend(label, _, rest) = cur {
        labels.push(label.clone());
        cur = rest;
    }
    labels.sort();
    assert_eq!(labels, ["children", "error", "status", "value"]);
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
fn builtin_spawn() {
    ok("let h = spawn { return 42 }; return $h");
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
fn service_is_external_on_a_bare_core_table() {
    // `service` is exarch's own surface (`SERVICE_BUILTIN`), never installed
    // into a core-only table.  The call resolves as an external command —
    // `String`, not the builtin's `Handle` return — so binding its result
    // where a `Handle` is expected is a static mismatch: the checker is
    // honest about what this session can actually run, not hard-coding the
    // name.  The positive half — the same call typechecking once a shell
    // installs `SERVICE_BUILTIN` — lives in exarch's suite.
    has_error(
        r#"let h = service "birth" { return 1 }; cancel $h"#,
        "couldn't match",
    );
}

#[test]
fn spawned_pipeline_types_as_handle() {
    // `spawn` suspends the pipeline; the binding is a Handle whose payload is
    // the pipeline's return type, recovered through `await`.
    ok("let h = spawn { echo hi }\nlet r = await $h\nreturn $r[value]");
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
fn a_value_returning_producer_is_an_ordinary_stage() {
    // A producer need not write.  `return hello`'s value is discarded
    // because the stage is non-final, and `echo` reads EOF.
    ok("return hello | echo");
}

#[test]
fn a_stage_still_wanting_an_argument_is_rejected() {
    // `length` is `String -> …`, a function, not a computation that can run.
    // As a bare builtin it names itself, the same rule a discarded statement
    // follows, rather than reporting an anonymous shape mismatch.
    let codes: Vec<_> = raw_errors("echo foo | length")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0050"]);
}

/// A stage-root `< f` or `<< w` answers every read the stage makes, for the
/// stage's whole run, so the producer across the `|` writes for nobody.  The
/// refusal names both rewrites that keep every command already written.
#[test]
fn a_pipe_into_a_stage_that_binds_its_own_stdin_is_refused() {
    has_error(
        "echo hi | cat < notes.txt",
        "the pipe writes into a stdin this `<` replaces",
    );
    has_error(
        "echo hi | from-string << #'body'#",
        "the pipe writes into a stdin this `<<` replaces",
    );
    assert!(
        has_hint(
            &raw_errors("echo hi | cat < notes.txt"),
            "run the stage's producer as its own statement",
        ),
        "the refusal must name the rewrite that keeps the producer"
    );
}

/// The rule is about the stage's root, which is the whole of what the pipeline
/// rule can see.  A first stage's stdin is nobody's output, and a read one
/// level in answers its own command's reads alone — not statically dead.
#[test]
fn stdin_bound_off_the_stage_root_is_left_alone() {
    ok("cat < notes.txt | from-string");
    ok("echo hi | !{ cat < notes.txt ; from-string }");
}

/// Each rewrite the refusal names keeps every command the program wrote.
#[test]
fn the_dead_edges_remedies_are_well_typed() {
    ok("cat < notes.txt");
    ok("echo hi ; cat < notes.txt");
    ok("let h = spawn { echo hi }; cat < notes.txt");
}

#[test]
fn pipeline_var_tail_stays_shape_forcing_not_pinned() {
    // A forced variable can still become a byte-reading stage at the edge;
    // the edge pins its input rather than classifying it as an application.
    ok("let ff = { |k| echo a | !$k }; ff { /bin/cat }");
}

// A block stage's byte channels are the join over *all* its statements, not
// just the last.  The output half was always joined; the input half was read
// off the final statement alone, so a reader anywhere but last vanished from
// the stage's type and its upstream was rejected as a `Bytes`→`∅` adjacency.

#[test]
fn block_stage_reading_bytes_before_its_last_statement_is_byte_input() {
    ok("echo foo | within [env: [X: 'y']] { from-lines; return () }");
}

/// A block *literal* in stage position is an ordinary value: the stage
/// returns the thunk and runs nothing.  Forcing it (`!$reader`) is what makes
/// it a computation that runs as the stage and reads the pipe.
#[test]
fn a_block_literal_stage_returns_a_thunk_and_a_forced_one_runs() {
    // Both typecheck.  They differ in what they *do*: the forced block runs
    // as a stage and decodes the bytes; the literal is an ordinary value, so
    // nothing runs and `input.txt` goes unread.
    ok("let reader = { from-line }\ncat input.txt | !$reader");
    ok("cat input.txt | { from-line }");
}

/// A `let` captures its RHS's *output* into the bound value, but its demand on
/// stdin belongs to the stage the binder shares that stdin with.
#[test]
fn bound_reader_before_the_last_statement_is_byte_input() {
    ok("echo foo | within [env: [X: 'y']] { let s = !{from-lines}; stream-to-list $s }");
}

/// A stage that ignores stdin is not thereby an error: the bytes are unread,
/// which is an ordinary thing for a byte stream to be.
#[test]
fn a_stage_that_never_reads_its_bytes_is_still_a_stage() {
    ok("echo foo | within [env: [X: 'y']] { let n = 1; length $n }");
}

// ─── String interpolation ─────────────────────────────────────────────────────

#[test]
fn interpolation_no_error() {
    ok("let x = world; return \"hello $x\"");
}

// ─── Head-not-invocable (T0011, surface phrasing) ──────────────────────────────

/// `'foo' bar baz` — a quoted string in command position with arguments.
/// The diagnostic must talk about the head being non-invocable, not about
/// `Cmd a vs a → b` jargon nor about an argument-type mismatch.
#[test]
fn head_not_invocable_string_with_args() {
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
fn head_not_invocable_span_covers_whole_command() {
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

/// A bound non-invocable value (`let x = 42; $x foo`) must trip the same
/// diagnostic — the value is data, not a function.
#[test]
fn head_not_invocable_int_variable_with_args() {
    has_error("let x = 42\n$x foo", "cannot be used as a command head");
}

/// Nested unescaped `"` inside a `"..."` string silently splits the line
/// into [string, $deref, string], producing a head-not-invocable error far
/// from the actual mistake.  The hint must point at the real cause —
/// nested double quotes — not the generic "wrap a invocable" advice.
#[test]
fn head_not_invocable_nested_double_quotes_hint() {
    let src = r#"let p = less
let m = "sh -c 'bat --pager="$(p)" --width=80'""#;
    let errs = raw_errors(src);
    let hint = errs
        .iter()
        .find_map(ral_core::TypeError::hint)
        .expect("expected a hint on the head-not-invocable diagnostic");
    assert!(
        hint.contains("nested double quotes"),
        "hint should mention nested double quotes, got: {hint:?}"
    );
}

/// The generic `'foo' bar baz` mistake — bare-word args, not quoted —
/// must keep the original hint, not the nested-quote one.
#[test]
fn head_not_invocable_bare_args_keeps_generic_hint() {
    let errs = raw_errors("'foo' bar baz");
    let hint = errs
        .iter()
        .find_map(ral_core::TypeError::hint)
        .expect("expected a hint on the head-not-invocable diagnostic");
    assert!(
        hint.contains("function or a thunk"),
        "hint should be the generic head-not-invocable advice, got: {hint:?}"
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
        .find_map(ral_core::TypeError::hint)
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
fn case_open_scrutinee_absorbs_arm_labels() {
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
        "no arm for `err",
    );
}

#[test]
fn case_arm_payload_mismatch() {
    // The `ok arm uses its payload as a String (via `upper`), but the
    // scrutinee's `ok was constructed with an Int payload — per-label
    // unification surfaces a type mismatch.
    has_error(
        "let r = `ok 5\nlet x = case $r [`ok: { |s| !{upper $s} }, `err: { |_| !{upper hello} }]\nreturn $x",
        "couldn't match",
    );
}

#[test]
fn case_arm_naming_a_non_function_is_faulted_as_an_arm() {
    // The arm runs what it names on the payload, so a value there is the
    // same error as a value in head position — but the user wrote an arm,
    // and the guidance has to be about arms.
    let errs = raw_errors("case `a () [`a: 7]");
    assert!(
        has_hint(&errs, "write the arm out"),
        "expected the arm-vocabulary hint, got: {errs:?}"
    );
    // A non-function head *inside* an arm the user wrote out is an ordinary
    // command-head fault: the arm's body is where it belongs.
    let errs = raw_errors("let n = 7\ncase `a () [`a: { |_| $n x }]");
    assert!(
        has_hint(&errs, "a command head must be"),
        "expected the command-head hint, got: {errs:?}"
    );
}

#[test]
fn case_arm_hint_survives_a_handler_that_hoists() {
    // A handler atom with an effect of its own is bound *ahead* of the
    // dispatch, so the arm's application sits under a `Bind` chain rather than
    // at the arm's root.  The arm still owns the head, and still says so.
    let errs = raw_errors(r#"case `a () [`a: "v$[1 + 1]"]"#);
    assert!(
        has_hint(&errs, "write the arm out"),
        "expected the arm-vocabulary hint, got: {errs:?}"
    );
    // And the arm's voice stops at a value: the block interpolated into that
    // handler is code of its own, so its bad head is diagnosed as any other.
    let errs = raw_errors("let n = 7\ncase `a () [`a: \"v!{$n x}\"]");
    assert!(
        has_hint(&errs, "a command head must be") && has_hint(&errs, "write the arm out"),
        "expected both hints, each in its own voice, got: {errs:?}"
    );
}

#[test]
fn case_arms_disagree_on_result() {
    // The two arms return values of different types — the shared
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
    ok("let f = { |n| if $[$n == 0] { return 0 } else { return !{f $[$n - 1]} } }\nreturn ()");
}

#[test]
fn recursive_stream_consumer() {
    // Pattern lifted from the streaming plan: a consumer that cases on a
    // forced thunk and recurses through the `more arm's payload.
    ok(
        "let drain = { |s| case !$s [`more: { |p| !{drain $p[tail]} }, `done: { |_| return () }] }\nreturn ()",
    );
}

#[test]
fn recursive_stream_producer_typechecks() {
    // The canonical infinite-producer pattern.  Phase C's equi-recursive
    // comp types let the cycle CompVar ⟶ Fun(Int, F (Variant {`more:
    // {head: Int, tail: Thunk(CompVar)} | `done | row})) close in the
    // union-find without tripping an occurs check.
    ok("let nats = { |n| stream-cons $n { !{nats $[$n + 1]} } }\nreturn ()");
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
fn recursive_sibling_arm_outputs_stay_independent_under_the_join() {
    // A `case` over one recursive group: `f` turns out byte-emitting, `g`
    // silent.  The arms' output join is bytes-dominant on the *case* only;
    // it must not equate the two still-open sibling outputs at visit time,
    // or `f`'s later `echo` grounds `g`'s output `Bytes` and `g`'s pure body
    // fails its own group unification.
    ok(
        "let h = { |v| case $v [ `go: { |_| !{f 0} }, `alt: { |_| !{g 0} }, `stop: { |_| return () } ] }\n\
        let f = { |n| return $h; echo x; return () }\n\
        let g = { |n| return $h; return () }\n\
        return ()",
    );
}

#[test]
fn a_stage_may_return_a_thunk() {
    // `Return(V) | Return(Thunk)`: nothing makes a thunk a worse thing to
    // discard than an Int, and the lambda is never applied.
    ok("return `more [head: 1, tail: { return `ok 2 }] | { |v| echo $v }");
}

#[test]
fn a_stream_piped_whole_is_accepted_and_simply_discarded() {
    // A stream is a value, and a non-final stage's value goes nowhere.  The
    // program is silent rather than wrong — the footgun admitted in
    // exchange for a stage rule that reads types, not spellings.
    ok("let s = !{stream-cons 1 { !{stream-nil} }}\n\
         $s | { |e| return $[$e + 1] } | { |y| return $[$y * 10] }");
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
    ok(r"grant [exec: ['/usr/bin/': 'allow']] { echo hi }");
}

#[test]
fn grant_exec_mixed_policy_shapes_typecheck() {
    // The TUTORIAL §16 example: a subcommand list, an empty list, and an
    // inline string policy in one map.
    ok(r"grant [exec: [git: ['status', 'log'], make: 'allow', '/usr/bin/': 'allow']] { echo hi }");
}

#[test]
fn grant_fs_read_write_typechecks() {
    ok(r"grant [fs: [read: ['/home/project'], write: ['/tmp/build']], net: false] { echo hi }");
}

#[test]
fn grant_inner_value_errors_still_surface() {
    // The policy values stay runtime-dispatched, but their sub-expressions are
    // still inferred — a genuine type error inside a policy value is a static
    // error rather than being masked by the (now removed) outer schema clash.
    has_error(
        r"grant [exec: [git: $[true + 1]]] { echo hi }",
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

/// A handler under a native's name installs — no name admission — but the
/// bare head still resolves to the native: `length`'s `Int` scheme governs
/// `r`, not the arm, so arithmetic on the result typechecks.
#[test]
fn within_handler_for_native_installs_but_bare_head_still_resolves_the_native() {
    ok(
        r#"within [handlers: [length: { |xs| return "hi" }]] { let r = !{length [1, 2, 3]}; return $[$r + 1] }"#,
    );
}

/// `^length` skips the env and reaches the arm: its `String` return clashes
/// with arithmetic the native's `Int` scheme would have permitted.
#[test]
fn caret_reaches_a_handler_stacked_on_a_native_name() {
    has_error(
        r#"within [handlers: [length: { |xs| return "mocked" }]] { let r = !{^length hello}; return $[$r + 0] }"#,
        "couldn't match",
    );
}

// ─── handler / alias mode preservation ───────────────────────────────────────
//
// A handler arm (or alias body) for an unknown head `h` defines `h`'s modes:
// its spec is fully fresh, so the arm pins it, while its value type stays free.
// Reinterpreting a known head pins the arm's payload route to that head's, and
// a clash there is a `RouteMismatch`.  Pinning is an install-time property of
// the arm and the head alone; no pipeline takes part in it.

fn is_route_mismatch(src: &str) -> bool {
    raw_errors(src).iter().any(|e| {
        matches!(
            e.kind,
            ral_core::typecheck::TypeErrorKind::RouteMismatch { .. }
        )
    })
}

/// A value-output `within` handler arm defines an unknown head's modes, so a
/// standalone use typechecks; the mismatch is a connection-point property,
/// surfacing only when the head's `∅` output feeds a byte consumer.
#[test]
fn within_handler_value_arm_pipes_into_a_decoder_freely() {
    // The arm defines the unknown head's route, and feeding a decoder says
    // nothing about it: `from-json` reads the byte channel, which is empty.
    ok(r"within [handlers: [foo: { |args| return 3 }]] { foo | from-json }");
    ok(r"within [handlers: [foo: { |args| return 3 }]] { foo }");
}

/// A byte-output `within` handler arm defines the unknown head as
/// byte-output and typechecks.
#[test]
fn within_handler_byte_output_arm_ok() {
    ok(r"within [handlers: [foo: { |args| echo hi }]] { foo }");
}

/// A stacked `echo` arm pins to the base frame's `None → Bytes` modes; a
/// value-output body is refused at install.
#[test]
fn within_handler_for_echo_breaking_its_route_is_rejected() {
    assert!(
        is_route_mismatch(
            r#"within [handlers: [echo: { |args| return "not bytes" }]] { echo hi }"#
        ),
        "expected a RouteMismatch pinning the echo arm to its head's byte route"
    );
}

/// The dual case: an arm that preserves `echo`'s byte mode installs cleanly.
#[test]
fn within_handler_for_echo_preserving_its_byte_mode_ok() {
    ok(r"within [handlers: [echo: { |args| echo mocked }]] { echo hi }");
}

/// WF-2 — a byte route pairs with a `Unit` value — is not carried by the type,
/// so it is an obligation on every operation that *grounds* a route to
/// `Bytes`.  The pin is one such grounder, and this arm is the shape that
/// catches it: `fail` never returns, so the callback's route is fresh;
/// `fold-lines` forwards it; the arm is `F[ρ] Int` with `ρ` still open when
/// the pin grounds it beside the head's `Bytes`.
///
/// Unrepaired this was silent: a release build exited 0 printing `""` where
/// the checker said `Int`, because the only tripwire was a `debug_assert!`
/// inside `Capture`.
#[test]
fn an_open_route_pinned_to_a_byte_head_must_return_unit() {
    has_error(
        "alias echo { |args| fold-lines { |a l| fail [status: 5] } 0 }\nreturn ()",
        "couldn't match",
    );
    // The diagnostic must name what the author did, not merely the clash.
    let errs =
        raw_errors("alias echo { |args| fold-lines { |a l| fail [status: 5] } 0 }\nreturn ()");
    assert!(
        errs.iter().any(|e| e
            .hint()
            .is_some_and(|h| h.contains("no separate value to return"))),
        "expected the WF-2 hint naming both sides, got: {errs:?}"
    );
    // The same arm returning `Unit` is consistent, and stays accepted.
    ok("alias echo { |args| fold-lines { |a l| fail [status: 5] } () }\nreturn ()");
}

/// A byte-output forwarding alias defines the unknown head as byte-output and
/// typechecks.
#[test]
fn alias_byte_output_forwarder_typechecks() {
    ok(r"alias myecho { |a| /bin/echo ...$a }; myecho hi");
}

/// A value-returning alias body defines the unknown head's route, so the
/// definition typechecks on its own.
#[test]
fn alias_value_output_body_typechecks() {
    ok("alias foo { |args| return 3 }\nreturn ()");
}

#[test]
fn alias_value_arm_piped_into_a_decoder_is_accepted() {
    // The alias binds for the following statements, and piping it into a
    // decoder is an ordinary byte edge: `from-json` reads EOF.
    ok("alias foo { |args| return 3 }\nfoo | from-json");
}

/// An alias over an existing alias resolves the head's route from the prior
/// alias's handler scheme and typechecks: a byte route pins to a byte route.
#[test]
fn alias_over_route_preserving_alias_typechecks() {
    ok(r"alias one { |args| echo one }; alias two { |args| one }; two");
}

/// The catch-all `handler: { comp }` names no specific head, so the
/// route-preservation rule does not constrain it — a value-returning
/// catch-all arm is not rejected.
#[test]
fn catch_all_handler_arm_is_not_route_pinned() {
    ok(r"within [handler: { |n a| return 'x' }] { return () }");
}

#[test]
fn local_binding_beats_handler() {
    ok(
        r"within [handlers: [foo: { |args| echo handler }]] { let foo = { return 41 }; let r = !{foo}; return $[$r + 1] }",
    );
}

#[test]
fn caret_skips_binding_but_not_handler() {
    has_error(
        r#"within [handlers: [cat: { |args| return "mocked" }]] { let r = !{^cat nope}; return $[$r + 0] }"#,
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
/// `infer_within_opts` produces no binding for `foo`.  The
/// body's call `foo 1` falls through to the builtin registry / external
/// path and typechecks without error (the name is unrecognised so a fresh
/// type is assigned).
#[test]
fn within_handler_non_literal_value_falls_through() {
    ok(r"let h = { |x| return $[$x + 1] }; within [handlers: [foo: $h]] { foo 1 }");
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
// An args-ignoring alias is still typed as a handler entry, not a CBPV
// function: the arm is a unary lambda whose argv parameter the body
// ignores; extra argv is inferred for local errors and then consumed by
// the static handler-call rule. The handler's return type is still pinned.

/// An args-ignoring alias binds in `TyEnv`:
/// `alias greet { |args| return "hi" }; greet` typechecks without error.
/// If the body returned an Int, using the result in a String context
/// (e.g. `str_concat "x" (greet)`) would be a type error; here we verify
/// the positive case and probe the inferred type by checking that using
/// the result in Int arithmetic IS a type error (because it's a String,
/// not an Int).
#[test]
fn alias_ignoring_args_binds_in_tyenv() {
    // Positive: the seq typechecks.  The arm is byte-output (`echo`),
    // preserving the head's `F[μ, Bytes]` spec, while its value type stays
    // String — a handler reinterprets a head without retyping its modes.
    ok(r#"alias greet { |args| echo hi; return "hi" }; greet"#);
    // Probe: the result is String, so using it in Int arithmetic is a type
    // error.  This would NOT error if the alias binding were absent (the
    // call would fall through to the external dispatcher and get a fresh
    // type variable).
    has_error(
        r#"let r = !{ alias greet { |args| echo hi; return "hi" }; greet }; return $[$r + 0]"#,
        "couldn't match",
    );
    has_error(
        r#"let r = !{ alias greet { |args| echo hi; return "hi" }; greet extra args }; return $[$r + 0]"#,
        "couldn't match",
    );
}

#[test]
fn value_lookup_does_not_reify_aliases_or_command_only_builtins() {
    has_error(
        r#"alias greet { |args| return "hi" }; let f = $greet; return $f"#,
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
        r"alias add1 { |n| return $[$n + 1] }; add1 5",
        "couldn't match",
    );
}

/// An alias arm's parameter *is* the argv, so its elements are text: `$a[0]` is
/// a `String`, and an arm that wants a number parses one.  Arithmetic straight
/// on an element is an error at the arm, which no call site can repair —
/// whatever was written, the arm consumes the rendering.
#[test]
fn an_alias_arm_parses_its_argv_to_get_a_number() {
    for call in ["inc 5", "inc hello"] {
        has_error(
            &format!(r"alias inc {{ |a| return $[$a[0] + 1] }}; {call}"),
            "couldn't match",
        );
    }
    // Parsed, the arm is well-typed, and takes either spelling: whether the
    // text is a number is `int`'s refusal to make at run time, not a type error.
    ok(r"alias inc { |a| return $[!{int $a[0]} + 1] }; inc 5");
    ok(r"alias inc { |a| return $[!{int $a[0]} + 1] }; inc hello");
}

/// Last-pushed alias shadows earlier alias at typecheck: the second
/// `alias greet` re-binds in `TyEnv` (last-pushed wins).  The final `greet`
/// should see the second alias's Int return type.
#[test]
fn alias_last_pushed_shadows_earlier() {
    // Both aliases registered; the second one (returning Int) should win.
    // Positive: Int arithmetic on the second alias's return value is fine
    // (verifies the second binding actually wins and returns Int).
    ok(
        r#"alias greet { |args| echo hi; return "hi" }; alias greet { |args| echo 42; return 42 }; let r = !{greet}; return $[$r + 0]"#,
    );
    // Negative: if only the first alias (String) won, the Int arithmetic
    // would error.  Since the second wins (Int), arithmetic succeeds — and
    // using the result as if it were a List would be an error.
    // Use `return $[$r + 0]` vs `return $[$r + true]` (Bool vs Int mismatch)
    // to confirm the type is pinned to Int, not a free variable.
    has_error(
        r#"alias greet { |args| echo hi; return "hi" }; alias greet { |args| echo 42; return 42 }; let r = !{greet}; return $[$r + true]"#,
        "couldn't match",
    );
}

/// Alias inside a conditional does NOT leak to subsequent Seq statements.
/// The alias is inside an `if` branch, not at Seq level, so the follow-up
/// call `greet` does not see the `TyEnv` binding.  Assert: no panic; the
/// program compiles.  (The call falls through to the external dispatcher
/// and gets a fresh type.)
#[test]
fn alias_inside_conditional_does_not_leak() {
    // The alias is inside the `then` branch — not at the enclosing Seq level.
    // The subsequent `greet` should NOT see the binding.  Compilation must
    // not panic; the type of `greet` is left free (external dispatch).
    ok("if true { alias greet { |args| return \"hi\" } } else { return () }; greet");
}

/// Multi-statement Seq: bindings from an alias are visible to all subsequent
/// statements, including an intermediate `let` and the final expression.
#[test]
fn alias_binding_visible_to_all_subsequent_statements() {
    // `r` is bound to the result of `greet` (String); the final `greet` also
    // sees the binding.  Both should typecheck without error.
    ok(r#"alias greet { |args| echo hi; return "hi" }; let r = !{greet}; greet"#);
    // Verify `r` really has String type by checking arithmetic on it errors.
    has_error(
        r#"let _ = !{ alias greet { |args| echo hi; return "hi" }; let r = !{greet}; return $[$r + 0] }; return ()"#,
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
    ok("alias g { |args| echo 42; return 42 }; return $[!{g} + 1]");

    // Unalias recognised.
    ok("alias g { |args| echo 42; return 42 }; unalias g; g");

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
        r"alias greet { |args| echo 41; return 41 }; unalias greet; let r = !{greet}; return $[$r + 0]",
        "couldn't match",
    );
    ok(
        r"within [handlers: [greet: { |args| echo 41; return 41 }]] { unalias greet; let r = !{greet}; return $[$r + 1] }",
    );
}

#[test]
fn builtin_command_signatures_are_explicit() {
    ok("range 1 3");
    has_error(r#"range "a" 3"#, "couldn't match");
    ok("from-lines");
    has_error("fail [status: 0]", "fail requires a nonzero status");
}

/// `fail` takes an error record, so a bare status — or a record without one —
/// is a static error rather than a runtime one.  The tail is open: a caught
/// error re-raises with the fields `try` gave it, and extra fields are the
/// point of the shape, not a violation of it.
#[test]
fn fail_demands_an_error_record() {
    ok("fail [status: 1]");
    ok(r#"fail [status: 2, message: "boom"]"#);
    ok(r#"fail [status: 2, message: "boom", cmd: "deploy", attempt: 3]"#);
    ok("try { fail [status: 1] } { |e| fail $e }");

    has_error("fail 1", "couldn't match");
    has_error(r#"fail "boom""#, "couldn't match");
    has_error(r#"fail [message: "boom"]"#, "no field named 'status'");
    has_error(r#"fail [status: "one"]"#, "couldn't match");
}

/// The `message` field is `String` or `Bytes` — a union the row cannot spell,
/// so the checker judges it once the record's shape is known.  An unresolved
/// field is left alone: both spellings run, and picking one would be a guess.
#[test]
fn fail_message_must_be_text() {
    ok("let m = !{ints-to-bytes [104, 105]}; fail [status: 1, message: $m]");
    ok("let f = { |m| fail [status: 1, message: $m] }; return ()");

    has_error("fail [status: 1, message: 42]", "must be a String or Bytes");
    has_error(
        "let e = try { fail [status: 1] } { |e| return $e }\nfail [...$e, message: 42]",
        "must be a String or Bytes",
    );
}

// ─── IR route annotation ──────────────────────────────────────────────────────
//
// The annotation pass writes the checker's ground verdicts into a rebuilt IR
// as explicit syntax: one `PipeYield` per pipeline, and a `Capture` node
// wherever a value boundary meets a byte route.  Schemes stay on the
// top-level spine only; the yields and the captures go everywhere, at any
// depth.

use ral_core::ir::{Comp, CompKind, IrPattern, PipeYield};

/// Compile `src` to an annotated comp, asserting it type-checks.
fn annotated(src: &str) -> Comp {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    let comp = elaborate(&ast, std::collections::HashSet::default(), "")
        .unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
    typecheck(
        &comp,
        ral_core::SessionSchemes::from_schemes(
            common::prelude_schemes(),
            ral_core::HostSurface::default().builtin_table(),
        ),
    )
    .unwrap_or_else(|errs| panic!("expected no errors in {src:?}, got: {errs:?}"))
}

/// Every `Pipeline` node's stage count and yield, anywhere in the tree.
fn all_pipeline_yields(comp: &Comp) -> Vec<(usize, PipeYield)> {
    let mut out = Vec::new();
    common::walk_comp(comp, &mut |c| {
        if let CompKind::Pipeline { stages, yields, .. } = &c.item {
            out.push((stages.len(), *yields));
        }
    });
    out
}

/// Every `Pipeline` node's `stage_types` slot, reachable anywhere.
fn all_pipeline_stage_types(comp: &Comp) -> Vec<(usize, Vec<Ty>)> {
    let mut out = Vec::new();
    common::walk_comp(comp, &mut |c| {
        if let CompKind::Pipeline {
            stages,
            stage_types,
            ..
        } = &c.item
        {
            out.push((stages.len(), stage_types.clone()));
        }
    });
    out
}

#[test]
fn top_level_pipeline_retains_per_stage_value_types() {
    let comp = annotated(r"/bin/echo hi | /bin/cat");
    let pipelines = all_pipeline_stage_types(&comp);
    assert_eq!(pipelines.len(), 1, "expected exactly one pipeline node");
    let (stage_count, types) = &pipelines[0];
    assert_eq!(*stage_count, 2, "two-stage pipeline");
    assert_eq!(types.len(), *stage_count, "one value type per stage");
    // Pins: each stage's value type is `Unit`, its bytes now being `result`.
    assert_eq!(types[0], Ty::Unit, "stage 0 value type retained");
    assert_eq!(types[1], Ty::Unit, "stage 1 value type retained");
}

#[test]
fn a_pipeline_carries_one_yield() {
    // One annotation for the whole pipeline, not one per stage: only the
    // final stage's route is ever read, and every interior edge is allocated
    // from position.  An external tail is captured from stdout, so the form
    // has nothing to hand back.
    assert_eq!(
        all_pipeline_yields(&annotated(r"/bin/echo hi | /bin/cat")),
        vec![(2, PipeYield::Unit)]
    );
    // A decoder tail returns its value instead, so the same shape of
    // pipeline yields that value.
    assert_eq!(
        all_pipeline_yields(&annotated(r"/bin/echo hi | from-string")),
        vec![(2, PipeYield::Last)]
    );
}

#[test]
fn pipeline_inside_thunk_body_is_annotated() {
    // The pipeline lives in a lambda body under a `let`; the pass must
    // descend past the spine to reach it.
    assert_eq!(
        all_pipeline_yields(&annotated(r"let f = { |x| /bin/echo $x | /bin/cat }")),
        vec![(2, PipeYield::Unit)]
    );
}

#[test]
fn a_byte_routed_bind_rhs_is_wrapped_in_capture() {
    // A byte-routed RHS (`echo`) is wrapped in the capture coercion — the
    // kernel node for the exact bytes, wrapped in a `Decode` node for the
    // text; a value-routed one (`return 42`) is left alone.  This is the whole
    // observable content of the route at a value boundary.
    let comp = annotated(r"let x = echo hi; let y = return 42; return ()");
    let mut binds = Vec::new();
    common::walk_comp(&comp, &mut |c| {
        if let CompKind::Bind {
            comp: rhs,
            pattern,
            ..
        } = &c.item
            && let IrPattern::Name(name) = pattern.as_ref()
        {
            let coerced = matches!(&rhs.item, CompKind::Decode(inner)
                if matches!(inner.item, CompKind::Capture(_)));
            binds.push((name.clone(), coerced));
        }
    });
    assert_eq!(
        binds,
        vec![("x".to_string(), true), ("y".to_string(), false)]
    );
}

/// Whether the `x` bind of `src` had its RHS captured — the whole observable
/// content of a `Bytes` route at a value boundary.
fn bind_x_is_captured(src: &str) -> bool {
    let comp = annotated(src);
    let mut found = None;
    common::walk_comp(&comp, &mut |c| {
        if let CompKind::Bind {
            comp: rhs,
            pattern,
            ..
        } = &c.item
            && let IrPattern::Name(name) = pattern.as_ref()
            && name == "x"
        {
            // A join's byte-side arms are captured individually (per-arm, not
            // at the join's own node), so the verdict is "any Capture in the
            // RHS subtree", not "the RHS itself is one".
            let mut has_capture = false;
            common::walk_comp(rhs, &mut |c| {
                if let CompKind::Capture(_) = &c.item {
                    has_capture = true;
                }
            });
            found = Some(has_capture);
        }
    });
    found.expect("a bind named `x`")
}

#[test]
fn if_byte_branches_join_to_byte_output() {
    // Both arms emit bytes, so the conditional's output channel is `Bytes`.
    assert!(bind_x_is_captured(
        "let x = if true { echo a } else { echo b }; return ()"
    ));
}

#[test]
fn if_value_branches_join_to_value_output() {
    // Both arms are pure values, so the conditional carries no byte output.
    assert!(!bind_x_is_captured(
        "let x = if true { return 1 } else { return 2 }; return ()"
    ));
}

#[test]
fn an_arms_writes_are_not_the_conditionals_payload() {
    // One arm writes and both return: the join's route stays `Value`, the
    // bytes go to the stage's sink, and the edge to `cat` asks nothing.
    ok("if true { echo hi; return 1 } else { return 2 } | cat");
}

#[test]
fn an_arms_writes_do_not_make_its_returned_value_a_payload() {
    // Both arms write and both return; the join's route is `Value` either
    // way, and the bytes go to the stage's stdout sink — here, the pipe.
    ok("if true { echo a; return 1 } else { echo b; return 2 } | cat");
}

#[test]
fn a_bind_rhs_writes_reach_the_stages_sink() {
    // The RHS's bytes escape the bind and reach the stage's visible stream,
    // which inside a pipeline is the pipe.  The returned value is discarded.
    ok("!{ let x = !{ echo hi; return 5 }; return () } | cat");
}

#[test]
fn a_captured_bind_rhs_keeps_its_bytes_out_of_the_pipe() {
    // `Capture` swallows the RHS's bytes into `x`, so this stage writes
    // nothing.  It typechecks, and `from-string` decodes EOF at runtime —
    // the stage's silence is a fact about the run, not about the types.
    ok("!{ let x = echo hi; length $x } | from-string");
}

#[test]
fn chain_byte_arms_join_to_byte_output() {
    // Both arms emit bytes, so the chain's output channel is `Bytes`.
    assert!(bind_x_is_captured("let x = echo a ? echo b; return ()"));
}

#[test]
fn a_bind_reads_its_rhs_route_through_the_store() {
    // The join grounds `t`'s route to `Bytes` before the binder is reached,
    // so `extract_return` hands the pin a route that is a `Var` in shape and
    // `Bytes` in the store — it destructures a head-canonical `Return` and
    // resolves no further.  Reading the shape unifies a settled `Bytes` with
    // `Value`; reading the store captures, and `x` is the decoded `String`.
    assert!(bind_x_is_captured(
        "let f = { |t| if true { echo hi } else { !$t }; let x = !$t; return $x }"
    ));
    has_error(
        "let f = { |t| if true { echo hi } else { !$t }; let x = !$t; return $[$x + 1] }",
        "String",
    );
}

/// Every tag arm of the program's `case`, in source order, with whether that
/// arm carries a `Capture` — the byte-side coercion, which a join inserts per
/// arm rather than at its own node.
fn case_arms_captured(src: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    common::walk_comp(&annotated(src), &mut |c| {
        let CompKind::Case { arms, .. } = &c.item else {
            return;
        };
        for arm in arms {
            let mut captured = false;
            common::walk_comp(arm.body.comp(), &mut |c| {
                captured |= matches!(c.item, CompKind::Capture(_));
            });
            out.push((arm.tag.item.clone(), captured));
        }
    });
    out
}

#[test]
fn a_case_arm_is_coerced_by_its_route_not_its_spelling() {
    // Both arms write bytes and the `case` is bound, so both owe the
    // byte-side coercion.  The set of alternatives is syntax, but an arm's
    // body is a computation however it is spelled: an arm naming a handler
    // elaborates to that handler applied to the payload, so the `Capture`
    // lands inside it exactly as it lands inside an inline `echo`.  Were it
    // left bare, its bytes would escape the binding.
    assert_eq!(
        case_arms_captured(
            "let h = { |p| echo b }\n\
             let v = `some ()\n\
             let x = case $v [`some: $h, `none: { |p| echo z }]\n\
             return ()"
        ),
        vec![("some".to_string(), true), ("none".to_string(), true)]
    );
    // And the two spellings of the same arm agree, which is the property the
    // equivalence rests on — not merely that each is captured.
    assert_eq!(
        case_arms_captured(
            "let h = { |p| echo b }\n\
             let v = `some ()\n\
             let x = case $v [`some: { |p| $h $p }, `none: { |p| echo z }]\n\
             return ()"
        ),
        vec![("some".to_string(), true), ("none".to_string(), true)]
    );
}

#[test]
fn a_discarded_cases_arms_stay_uncaptured() {
    // The dual, for both spellings again: in statement position nothing
    // reads the payload, so no arm is coerced and each arm's bytes flush
    // through to the stage's sink.
    assert_eq!(
        case_arms_captured(
            "let h = { |p| echo b }\n\
             let v = `some ()\n\
             case $v [`some: $h, `none: { |p| echo z }]"
        ),
        vec![("some".to_string(), false), ("none".to_string(), false)]
    );
}

#[test]
fn seq_tail_still_unknown_may_become_a_function() {
    // The tail gives the sequence its value and may be a function: with
    // every statement silent, `!$f` must stay free to resolve `Fun` at the
    // call site rather than be forced into stage shape at visit time.  The
    // call's own result is bound (`let r = …`) rather than left bare, since a
    // program's own discarded value is now held to the same shape a pipeline
    // stage is — a distinct rule from the one this test pins.
    ok("let apply = { |f| return (); !$f }; let r = apply { |x| return $x }");
}

#[test]
fn a_preceding_statements_bytes_do_not_constrain_the_tail() {
    // The dual: a statement that reads bytes is still just a discarded
    // statement.  It leaves nothing behind for the tail to carry, so the
    // tail stays free to resolve `Fun` at the call site.  Bound for the same
    // reason as above.
    ok("let apply = { |f| let x = from-string; !$f }; let r = apply { |x| return $x }");
}

// ─── Observed-value arm join (if / `?` / try) ─────────────────────────────────
//
// One boundary judgment for arm joins: arms that all resolve to
// `CompTy::Return` observe their raw value under the arms' joined final
// output, then unify the observed values, instead of unifying raw values —
// the static image of `eval_bind_rhs`'s per-run `Unit` capture switch.

#[test]
fn if_mixed_external_and_echo_arms_accepted() {
    // `^cmd` (external, String) vs `echo` (raw Unit): both observe to
    // String under the joined Bytes output, so they unify.
    ok("if true { ^cmd } else { echo x }");
}

#[test]
fn chain_mixed_external_and_echo_arms_accepted() {
    ok("^cmd ? echo x");
}

#[test]
fn if_empty_else_arm_still_accepted() {
    // The empty else arm is pure `Unit`; `echo x` observes to String under
    // the joined Bytes output; the empty arm's raw `Unit` also observes to
    // String under that same joined output, so the arms unify.
    ok("if true { echo x } else {}");
}

#[test]
fn chain_return_int_then_echo_is_observed_mismatch() {
    // Pins: `return 1`'s `Int` can't subsume onto the byte side `echo x` grounds.
    has_error("return 1 ? echo x", "couldn't match");
}

#[test]
fn if_byte_arm_alongside_value_arm_is_rejected() {
    // Pins: a mixed byte/value join is rejected outright, not reconciled.
    has_error("if true { echo hi } else { return 's' }", "payload route");
}

#[test]
fn try_byte_body_alongside_value_handler_is_rejected() {
    has_error("try { echo hi } { |_| return 's' }", "payload route");
}

#[test]
fn sibling_arms_grounding_apart_report_under_the_joins_own_reason() {
    // Both recursive arms are still open when the `case` is visited; `f`
    // grounds `Bytes` and `g` grounds `∅`-at-`Int` only at their own bodies.
    // The clash is the join's to report — a conduit mismatch under
    // `CaseArms` — not a mismatch surfacing at whichever group unification
    // next touches a variable the join had eagerly equated.
    let errs = raw_errors(
        "let h = { |v| case $v [ `a: { |_| return () }, `b: { |_| !{f 0} }, `c: { |_| !{g 0} } ] }\n\
         let f = { |n| return $h; echo done }\n\
         let g = { |n| return $h; return 5 }\n\
         return ()",
    );
    assert!(
        errs.iter().any(|e| {
            matches!(e.reason, Some(ral_core::typecheck::Reason::CaseArms))
                && e.kind.render_message().contains("payload route")
        }),
        "expected the join's own conduit mismatch under CaseArms, got: {errs:?}"
    );
}

#[test]
fn inner_bind_does_not_collapse_the_enclosing_groups_join() {
    // The sibling-arms program with one tail's arithmetic hoisted by the
    // elaborator into a gensym `Bind`.  That inner boundary owns none of
    // the `case`'s variables, so the join must ride to the group fixpoint
    // and report there — the same conduit mismatch under `CaseArms` as the
    // unhoisted program, not a shape clash at the group unification.
    let errs = raw_errors(
        "let h = { |v| case $v [ `a: { |_| return () }, `b: { |_| !{f 0} }, `c: { |_| !{g 0} } ] }\n\
         let f = { |n| return $h; echo done }\n\
         let g = { |n| return $h; return $[$n + 1] }\n\
         return ()",
    );
    assert!(
        errs.iter().any(|e| {
            matches!(e.reason, Some(ral_core::typecheck::Reason::CaseArms))
                && e.kind.render_message().contains("payload route")
        }),
        "expected the join's own conduit mismatch under CaseArms, got: {errs:?}"
    );
}

#[test]
fn inner_bind_does_not_foreclose_a_siblings_subsumption() {
    // `g` ends `∅`-at-`Unit`, which subsumes beside `f`'s byte arm — but
    // only if the hoisted `Bind` inside `g`'s body leaves the still-open
    // join alone instead of concluding it early and pinning `g`'s arm
    // `Bytes` before its own body has spoken.
    ok(
        "let h = { |v| case $v [ `b: { |_| !{f 0} }, `c: { |_| !{g 0} } ] }\n\
         let f = { |n| return $h; echo done }\n\
         let g = { |n| return $h; let w = $[$n + 1]; return () }\n\
         return ()",
    );
}

#[test]
fn late_byte_arm_beside_value_payload_arm_is_a_conduit_mismatch_under_try_arms() {
    // The handler is `∅`-at-`Int` from the start; the body's `Bytes` result
    // arrives only when `f`'s own body is inferred.  The join must still be
    // open at that point, so the verdict is its conduit mismatch under
    // `TryArms` — not an early value-side pin of the body's result whose
    // clash then surfaces at the group unification.
    let errs = raw_errors(
        "let h = { |v| try { !{f $v} } { |_| return 5 } }\n\
         let f = { |n| return $h; echo x }\n\
         return ()",
    );
    assert!(
        errs.iter().any(|e| {
            matches!(e.reason, Some(ral_core::typecheck::Reason::TryArms))
                && e.kind.render_message().contains("payload route")
        }),
        "expected the join's own conduit mismatch under TryArms, got: {errs:?}"
    );
}

#[test]
fn traced_mixed_join_under_opaque_force_is_now_rejected() {
    // Pins: the soundness hole this pass closes — a mixed join under an
    // opaque force is now a static error, not a silent runtime mismatch.
    has_error(
        "let v = !{ echo pre; if true { echo hi } else { return 'other' } }",
        "payload route",
    );
}

#[test]
fn chain_return_string_then_return_int_is_static_mismatch() {
    // The 27e84d3f two-stage repro: both arms are pure returns, so the
    // joined output is `None` and each arm's raw type is its own observed
    // value — String vs Int is a static clash, not a runtime surprise.
    has_error("return hello ? return 1", "couldn't match");
}

#[test]
fn try_relaxation_echo_body_unit_handler_accepted() {
    // Previously rejected (observed String vs raw Unit): the body's `echo`
    // and the handler's `return ()` now observe under their joined Bytes
    // output, both landing on String.
    ok("try { echo x } { |_| return () }");
}

#[test]
fn byte_side_subsumption_does_not_depend_on_statement_order() {
    // `Value Unit ⊑ Bytes` demands that a subsumed arm's value be `Unit` —
    // an equation the byte side imposes, not one it asks after.  `$x`'s type
    // is still open when the byte join is reached in the first spelling and
    // already `Unit` in the second, and the two must agree.
    ok("let f = { |x| if true { return () } else { return $x }\n\
                     if true { echo hi } else { return $x } }");
    ok("let f = { |x| if true { echo hi } else { return $x }\n\
                     if true { return () } else { return $x } }");
}

#[test]
fn an_equation_added_elsewhere_does_not_make_a_program_typecheck() {
    // The same monotonicity from the other side: prefixing a binding that
    // only equates `$x`'s type with `Unit` adds no information the join did
    // not already have, so it cannot turn a rejection into an acceptance.
    ok("let f = { |t x| if true { !$t } else { return $x }\n\
                       let z = 1\n\
                       if true { echo hi } else { !$t }\n\
                       return $x }");
    ok(
        "let f = { |t x| let u = if true { return $x } else { return () }\n\
                       if true { !$t } else { return $x }\n\
                       let z = 1\n\
                       if true { echo hi } else { !$t }\n\
                       return $x }",
    );
}

#[test]
fn an_arms_unsolved_value_type_is_not_evidence_for_the_value_side() {
    // At the boundary that owns it the join's own result is still open, and
    // its one value-routed arm returns `$x` at a type nothing has solved.
    // Absence of a solution is not a payload: pinning the open arm to the
    // value side here would foreclose the byte side on no evidence.  `g`
    // stays route-polymorphic instead, and both uses type.
    ok("let g = { |t x| if true { !$t } else { return $x } }\n\
        g { echo hi } ()\n\
        g { return 3 } 4");
}

#[test]
fn a_route_polymorphic_join_still_carries_wf2_to_its_byte_uses() {
    // Route polymorphism does not loosen WF-2.  The join tied its arms'
    // value types together, and `{ echo hi }` is `U(F[Bytes] Unit)` whole,
    // so instantiating `g` at the byte side ties that shared type to `Unit`
    // and an `Int` argument is rejected.
    has_error(
        "let g = { |t x| if true { !$t } else { return $x } }\n\
         let y = g { echo hi } 3",
        "couldn't match",
    );
}

#[test]
fn nested_binds_carry_no_scheme_while_spine_does() {
    // The spine `let g` carries a scheme; a `let inner` under the lambda
    // body evaluates in a block scope and never installs, so it stays
    // `scheme: None`.
    let comp = annotated(r"let g = { |x| let inner = return $x; return $inner }; return ()");
    let mut spine_named = None;
    let mut nested_named = None;
    common::walk_comp(&comp, &mut |c| {
        if let CompKind::Bind {
            pattern,
            scheme,
            ..
        } = &c.item
            && let IrPattern::Name(name) = pattern.as_ref()
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

// ─── Nullary codec decoders ──────────────────────────────────────────────────
//
// Every `from-X` is `F[Bytes, ∅] A`: it consumes the byte channel and returns
// a value, so it has no argument slot to fill.  An argument is a static error
// (T0054), not a signal raised once the call is already running.

/// One T0054 per decoder, with the encoder-pipe remedy as its hint.
#[test]
fn decoder_with_an_argument_is_a_type_error() {
    for name in ["from-json", "from-string", "from-lines"] {
        let errs = raw_errors(&format!("let x = hello\n{name} $x"));
        let codes: Vec<_> = errs.iter().map(|e| e.kind.code()).collect();
        assert_eq!(codes, ["T0054"], "expected one T0054 for `{name} $x`");
        let hint = errs[0].hint().unwrap_or_default();
        assert!(
            hint.contains("to-string") && hint.contains("from-json"),
            "hint should point at the encoder pipe, got: {hint:?}"
        );
    }
}

/// A decoder tail carries its payload as a returned value, so the pipeline
/// it ends yields that value to the parent.
#[test]
fn a_decoder_tail_yields_the_pipelines_value() {
    assert_eq!(
        all_pipeline_yields(&annotated("echo hi | from-json")),
        vec![(2, PipeYield::Last)]
    );
}

/// A decoder is fixed arity zero, so it derives a value scheme like any
/// other fixed-arity native: `$from-json` is a first-class nullary thunk.
#[test]
fn decoder_is_a_first_class_nullary_native() {
    ok("let f = $from-json; return $f");
}

// ─── Codec encoders take their value as an argument ──────────────────────────
//
// Dually, every `to-X` writes the byte channel and takes the value to encode as
// its one argument.  Omitting it is an arity error (T0050); nothing supplies the
// argument from the channel or from an upstream stage.

/// One T0050 per encoder in the bare form: the missing argument is the whole
/// story, a program whose value is a discarded function.  `echo !{name}`,
/// once tested here too, no longer is one: under currying, `!{to-json}` is
/// the function itself, handed to `echo`, which renders it like any other
/// native (`echo !{length}` always has).
#[test]
fn encoder_without_its_value_is_an_arity_error() {
    for name in [
        "to-bytes",
        "ints-to-bytes",
        "to-string",
        "to-line",
        "to-lines",
        "to-json",
        "to-csv",
    ] {
        let codes: Vec<_> = raw_errors(name).iter().map(|e| e.kind.code()).collect();
        assert_eq!(codes, ["T0050"], "expected one T0050 for {name:?}");
    }
}

/// A pipe carries bytes, so an upstream stage is no argument either: the
/// encoder feeding an external consumer names its own value.  Under-application
/// is silent now (pure FP rules — currying), but a stage must still be ready
/// to run, and a stage that is a bare short builtin names itself — one named
/// diagnostic, better than either an unrelated one or none at all.
#[test]
fn an_encoder_stage_takes_its_value_not_the_upstream_one() {
    let codes: Vec<_> = raw_errors("[1, 2] | to-lines | grep .")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0050"]);
    ok("to-lines [1, 2] | grep .");
}

/// Written out, each encoder takes the value its registry doc promises.
#[test]
fn saturated_encoders_typecheck() {
    ok("echo !{ints-to-bytes [104, 105]}");
    ok("let b = !{echo hi | from-bytes}; echo !{to-bytes $b}");
    ok("echo !{to-string 'hi'}");
    ok("echo !{to-line 1}");
    ok("echo !{to-lines ['a', 'b']}");
    ok("echo !{to-json [a: 1]}");
    ok("echo !{to-csv [[a: 1]]}");
}

/// Two writers put bytes on the channel, and each names one argument.
/// `to-bytes` is `from-bytes`'s inverse and takes the `Bytes` it returns;
/// `ints-to-bytes` takes the numbers ral has no literal for.  Neither reads
/// the other's argument, and neither reads text.
#[test]
fn the_two_byte_writers_take_their_own_argument() {
    has_error("to-bytes hello", "couldn't match");
    has_error("to-bytes [104, 105]", "couldn't match");
    has_error("ints-to-bytes 'hi'", "couldn't match");
    has_error("ints-to-bytes ['x']", "couldn't match");
    has_error(
        "let b = !{echo hi | from-bytes}; ints-to-bytes $b",
        "couldn't match",
    );

    // Numbers handed to `to-bytes` are the one slip worth naming a rewrite for.
    let errs = raw_errors("to-bytes [104, 105]");
    assert!(
        has_hint(&errs, "ints-to-bytes"),
        "the hint should name the other writer, got: {errs:?}"
    );

    // The same mismatch at `to-lines` is not a `to-bytes` slip; the hint must
    // still fit.
    let errs = raw_errors("let b = !{echo hi | from-bytes}; to-lines $b");
    assert!(
        has_hint(&errs, "to-lines"),
        "the hint should name the writer that was called, got: {errs:?}"
    );
}

// ─── A spread is the notation of an argv ─────────────────────────────────────
//
// `...` splices a list into an argv, and only a command, an external, a base
// frame, or a handler arm has one.  A value takes its arguments by application,
// curried, at the arity its own type declares, so a spread in a value's
// argument position names nothing to fill: T0056.

/// A fixed-arity builtin declares its arity, so the diagnostic can name both it
/// and the rewrite.
#[test]
fn a_spread_into_a_fixed_arity_builtin_is_refused() {
    let errs = raw_errors("let xs = [1]; to-json ...$xs");
    let codes: Vec<_> = errs.iter().map(|e| e.kind.code()).collect();
    assert_eq!(codes, ["T0056"]);
    assert!(
        has_hint(&errs, "to-json $xs[0]"),
        "the hint should name the rewrite, got: {errs:?}"
    );
}

/// A bound lambda has no signature to name an arity, so the diagnostic speaks
/// of the head instead, and the hint sketches the rewrite's shape rather than
/// this call's own names.
#[test]
fn a_spread_into_a_bound_lambda_is_refused() {
    let errs = raw_errors("let g = { |x| return $x }; let ys = [1]; g ...$ys");
    let codes: Vec<_> = errs.iter().map(|e| e.kind.code()).collect();
    assert_eq!(codes, ["T0056"]);
    assert!(
        has_hint(&errs, "$f $xs[0]"),
        "the hint should sketch the rewrite, got: {errs:?}"
    );
}

/// The head may be a parameter, its arity unknown until the call site supplies
/// it.  The refusal is structural, so a value reached through a parameter is
/// refused where it is written, not where it is passed.
#[test]
fn a_spread_into_a_value_parameter_is_refused() {
    let codes: Vec<_> = raw_errors("let appl = { |f args| $f ...$args }; appl $to-json [1]")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0056"]);
}

/// Two spreads are one mistake, and one rewrite answers both.
#[test]
fn only_the_first_spread_of_a_call_is_reported() {
    let codes: Vec<_> = raw_errors("let xs = [1]; to-json ...$xs ...$xs")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0056"]);
}

/// An error inside a refused spread is still an error: the subexpressions are
/// inferred before the spread itself is judged.
#[test]
fn a_refused_spread_still_reports_errors_inside_it() {
    let codes: Vec<_> = raw_errors("let r = [a: 1]; to-json ...$r[missing]")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0020", "T0056"]);
}

/// A base frame takes an argv, and `...` is exactly its notation.  `detach` is
/// checked against a surface that installs it, because core's table alone
/// publishes only `echo`: read as an external, `detach` would accept the spread
/// for an external's reasons and say nothing about frames.
#[test]
fn spreads_into_a_base_frame_are_legal() {
    ok("let xs = [hello, world]; echo ...$xs");
    #[cfg(unix)]
    ok_against(
        "let xs = ['300']; detach #'a long sleep'# /bin/sleep ...$xs",
        &detach_surface(),
    );
}

/// An external's argv is the operating system's, so a spread is how a list
/// becomes it.
#[test]
fn a_spread_into_an_external_is_legal() {
    ok("let xs = ['-n', hi]; /bin/echo ...$xs");
}

/// A handler arm and an alias arm both take an argv, and both take a spread —
/// into the arm's own body, and into the head the arm defines.
#[test]
fn spreads_into_a_handler_or_alias_arm_are_legal() {
    ok(r"within [handlers: [foo: { |args| echo ...$args }]] { let xs = [1]; foo ...$xs }");
    ok(r"alias myecho { |a| echo ...$a }; let xs = [hi]; myecho ...$xs");
}

/// A list literal's spread and a rest pattern are a different construct
/// altogether — list surgery, not an argv — and the rule leaves them be.
#[test]
fn list_literal_spreads_and_rest_patterns_are_untouched() {
    ok("let xs = [1]; return [1, ...$xs, 2]");
    ok("let [first, ...rest] = [1, 2, 3]; return $rest");
}

// ─── `cd` names its directory ────────────────────────────────────────────────
//
// `cd` takes exactly one String path, and nothing stands in for it: no `$HOME`,
// no empty string.  So a bare `cd` is an arity error, and — a declared arity
// being a curry spine — `$cd` is a value like any other native's.

/// The arity error names the verb the user wrote and sends them to its own
/// shape, rather than speaking anonymously of "a builtin".
#[test]
fn a_bare_cd_is_an_arity_error_that_names_cd() {
    let errs = raw_errors("cd");
    let codes: Vec<_> = errs.iter().map(|e| e.kind.code()).collect();
    assert_eq!(codes, ["T0050"]);
    assert!(
        errs[0]
            .kind
            .render_message()
            .contains("`cd` expected 1 argument"),
        "the arity error must name `cd`, got: {errs:?}"
    );
    assert!(
        has_hint(&errs, "explain cd"),
        "the hint should point at the verb's own entry, got: {errs:?}"
    );
}

/// One String is what the slot holds, `~` included; an Int is not a path.
#[test]
fn cd_takes_one_string_path() {
    ok("cd '/tmp'");
    ok("cd ~");
    has_error("cd 3", "couldn't match");
}

/// A declared arity derives a value scheme, so `cd` is first-class — nameable
/// as `$cd` and applicable to its path.
#[test]
fn cd_is_a_first_class_native() {
    ok("let f = $cd; return $f");
    ok("let f = $cd; $f '/tmp'");
}

/// The path being an argument taken by application, a spread into `cd` names no
/// argv to fill.
#[test]
fn a_spread_into_cd_is_refused() {
    let codes: Vec<_> = raw_errors("let xs = ['/tmp']; cd ...$xs")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0056"]);
}

// ─── An argv, or arguments ───────────────────────────────────────────────────
//
// ral passes arguments two ways, and the two share nothing.  A handler arm, a
// base frame and an external are each variadic over an *argv*: one argument,
// `List String`, every element crossing rendered.  Everything else is lambda
// calculus — curried application at the arity its own type declares, and
// first-class as `$name`.  Which half a name is in is declared, never read off
// how it is called, so no call site moves it.

/// An argv is `List String` whatever was written into it, so a heterogeneous
/// call is the ordinary case: the elements are rendered on the way in, not
/// unified with one another.
#[test]
fn a_handler_arm_takes_a_heterogeneous_argv() {
    ok(r"within [handlers: [mycmd: { |args| echo ...$args }]] { mycmd hello 1 true }");
    ok(r"alias mycmd { |args| echo ...$args }; mycmd hello 1 true");
}

/// The dual, and the price of the rule: an element is text, so arithmetic on
/// one is an error at the arm.  An arm consumes what an exec call would, or it
/// is not substitutable for the command it stands in for.
#[test]
fn an_argv_element_is_text_whatever_was_written() {
    has_error(
        r"within [handlers: [mycmd: { |args| return $[$args[0] + 1] }]] { mycmd 1 }",
        "couldn't match",
    );
    // The argv is that list, too, not a scalar per atom — the diagnostic says so.
    has_error(r"alias f { |a| return !{upper $a} }; f x", "type [String]");
}

/// `echo` is a base frame: an argv in command position, and no value to hold.
#[test]
fn echo_is_a_base_frame_not_a_value() {
    let codes: Vec<_> = raw_errors("return $echo")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0042"]);
    ok("echo hello [a: 1] 3.5 true");
    ok("let xs = [1, 2]; echo ...$xs");
}

/// `detach` is the other frame core publishes, and a frame for the same reason:
/// it takes an argv, so there is no `$detach` either.  Checked against the
/// surface that installs it — the bare table would answer about an external.
#[cfg(unix)]
#[test]
fn detach_is_a_base_frame_not_a_value() {
    let codes: Vec<_> = errors_against("return $detach", &detach_surface())
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0042"]);
}

/// A user alias is a handler entry, so it is a name and not a value either —
/// the same refusal from the other half of name resolution, under its own code.
#[test]
fn an_alias_in_value_position_is_a_handler_not_a_value() {
    let codes: Vec<_> = raw_errors("alias foo { |a| echo hi }; return $foo")
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0041"]);
}

/// A base frame's scheme is seeded as a handler binding, so a pipeline reads
/// `echo`'s byte route off the frame itself.
#[test]
fn a_base_frame_routes_a_pipeline_as_bytes() {
    ok("echo hi | grep .");
    ok("let xs = [a, b]; echo ...$xs | grep .");
}

// ─── The exec boundary is gated, statically where it can be ──────────────────
//
// An argv the shell renders itself is total: every value has a text form, so a
// base frame and a handler arm refuse nothing.  An argv heading for `execve(2)`
// is a list of operating-system words, and some shapes are not words — a list is
// several arguments, a map is fields, a block has not run, bytes are a channel.
// Shape is exactly what a type states, so wherever the type is concrete that
// refusal is a diagnostic (T0057) rather than a failed run; where it is a
// variable, `runtime::command::vet` keeps the question.

/// Each refused shape, named where it is written, with the command named as the
/// spawn-time refusal names it.
#[test]
fn a_concretely_unrenderable_argument_to_an_external_is_refused() {
    for src in [
        "let xs = [1, 2]; /bin/echo $xs",
        "let r = [a: 1]; /bin/echo $r",
        "let k = 'a'; let m = [$k: 2]; /bin/echo $m",
        "let b = { echo hi }; /bin/echo $b",
        "let f = { |x| return $x }; /bin/echo $f",
        "let h = spawn { return 1 }; /bin/echo $h",
        "let b = !{echo hi | from-bytes}; /bin/echo $b",
    ] {
        let errs = raw_errors(src);
        let codes: Vec<_> = errs.iter().map(|e| e.kind.code()).collect();
        assert_eq!(codes, ["T0057"], "{src:?}");
        assert!(
            errs[0]
                .kind
                .render_message()
                .contains("to external command '/bin/echo'"),
            "the refusal must name the command, got: {errs:?}"
        );
    }
}

/// A bare name resolving to no binding, builtin or handler is an external, and
/// names itself in the refusal; a `~` head is named as the source wrote it,
/// there being no `HOME` to expand it against before the run.
#[test]
fn the_refusal_names_the_head_as_written() {
    for (src, head) in [
        ("let xs = [1]; mycmd $xs", "mycmd"),
        ("let xs = [1]; ~/bin/mycmd $xs", "~/bin/mycmd"),
    ] {
        let errs = raw_errors(src);
        assert!(
            errs.iter().any(|e| e
                .kind
                .render_message()
                .contains(&format!("to external command '{head}'"))),
            "expected the head {head:?} named, got: {errs:?}"
        );
    }
}

/// Each shape's hint names its own way down to words — the same sentence the
/// spawn-time refusal carries, so one mistake is described in one language
/// wherever it is caught.
#[test]
fn each_refused_shape_names_its_own_remedy() {
    for (src, fragment) in [
        ("let xs = [1, 2]; /bin/echo $xs", "...$xs"),
        ("let r = [a: 1]; /bin/echo $r", "to-json"),
        ("let b = { echo hi }; /bin/echo $b", "!{!$b}"),
        ("let h = spawn { return 1 }; /bin/echo $h", "await"),
        ("let b = !{echo hi | from-bytes}; /bin/echo $b", "to-bytes"),
    ] {
        assert!(
            has_hint(&raw_errors(src), fragment),
            "{src:?}: the hint should mention {fragment:?}"
        );
    }
}

/// Every remedy a hint names is itself a program that checks, so the guidance
/// can be followed as written.
#[test]
fn the_remedies_the_hints_name_are_well_typed() {
    ok("let xs = [1, 2]; /bin/echo ...$xs");
    ok("let m = [a: 1]; /bin/echo $m[a]");
    ok("let m = [a: 1]; /bin/echo !{to-json $m}");
    ok("let b = { echo hi }; /bin/echo !{!$b}");
    ok("let h = spawn { return 1 }; let r = await $h; /bin/echo $r[value]");
    ok("let b = !{echo hi | from-bytes}; to-bytes $b | /bin/cat");
}

/// What the type does not say, the checker does not say: a parameter's shape is
/// the run's business, and the pre-spawn gate is still there to refuse it.  This
/// is why the static gate costs no program that ran before.
#[test]
fn a_polymorphic_argument_is_left_to_the_run() {
    ok("let show = { |v| /bin/echo $v }; show [a: 1]");
    ok("let show = { |v| /bin/echo $v }; show { echo hi }");
}

/// A spread is left to the run for a second reason: how many elements it
/// contributes is dynamic, and an empty one contributes none — so a refusal here
/// could reject a call that spawns cleanly.
#[test]
fn a_spread_of_unrenderable_elements_is_left_to_the_run() {
    ok("let xss = [[1], [2]]; /bin/echo ...$xss");
}

/// The gate is the exec boundary's alone: an argv the shell renders itself takes
/// every shape, and `echo [a: 1]` must keep working.
#[test]
fn an_in_shell_argv_refuses_nothing() {
    ok("echo [a: 1]");
    ok("let f = { |x| return $x }; echo $f [1, 2] !{str 3}");
    ok(r"let f = { |x| return $x }; ^echo $f");
    ok(r"alias mycmd { |args| ^echo ...$args }; mycmd [a: 1]");
    ok(r"within [handlers: [mycmd: { |args| ^echo ...$args }]] { mycmd [a: 1] }");
}

// ─── Row termination and duplicate-key semantics ──────────────────────────────
//
// These pin three row-subsystem repairs: row unification must terminate with
// an infinite-row error, rather than looping forever, when a row cycle has no
// finite or rational solution — whether the cycle sits at the row spine or is
// reached through a field type — and duplicate keys in a record or case
// literal must resolve last-wins, matching the runtime, rather than
// first-wins.  Before the fix, the first two overflowed the host stack during
// `--check`; the third disagreed with the runtime on duplicate keys.

/// Both branches spread the *same* parameter row, so `merge_branches`
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

/// The else branch builds a record whose field type is itself a record
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

/// A duplicate explicit key resolves last-wins, matching the runtime
/// `Value::map`.  Here the last `x` is a String, so reading `$m[x]` in Int
/// arithmetic is a static type error — the same shape the runtime rejects.
#[test]
fn duplicate_key_last_wins_string_then_arith_is_error() {
    has_error(
        "let m = [x: 1, x: \"two\"]\nlet y = $[$m[x] + 1]\nreturn $y",
        "couldn't match",
    );
}

/// The complementary direction: the last `x` is an Int, so reading it in
/// Int arithmetic typechecks.  Under the old first-wins checker this was a
/// String-vs-Int error; last-wins makes it well-typed, consistent with the
/// runtime which would compute `2`.
#[test]
fn duplicate_key_last_wins_int_then_arith_ok() {
    ok("let m = [x: \"two\", x: 1]\nreturn $[$m[x] + 1]");
}

/// A `case` arm is *not* a record entry, so the last-wins rule stops at the
/// eliminator: exactly one computation may run per tag, and a repeated arm is
/// refused outright rather than silently resolved to the later one.  The
/// refusal is the parser's, since it needs no types to see it.
#[test]
fn duplicate_case_arm_is_refused() {
    let err = parse(
        "let r = `ok 5\n\
         let x = case $r [`ok: { |s| !{upper $s} }, `ok: { |i| return $[$i + 1] }, `err: { |_| return 0 }]\n\
         return $x",
    )
    .expect_err("a repeated `case` arm must not parse");
    assert!(
        err.message.contains("already has a `ok arm"),
        "expected the duplicate-arm refusal, got: {}",
        err.message
    );
}

