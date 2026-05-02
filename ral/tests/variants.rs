//! End-to-end tests for variants and tag-keyed records (Phase A).
//!
//! These exercise the parser, elaborator, and runtime.  Pure typing
//! behaviour (e.g. variant inference at an open row) is in
//! `core/tests/typecheck.rs`.

mod common;

#[test]
fn variant_displays_with_payload() {
    let out = common::run("variant_with_payload", "let x = .ok 42\necho $x\n");
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), ".ok 42");
}

#[test]
fn variant_nullary_displays_as_dot_label() {
    let out = common::run("variant_nullary", "echo .none\n");
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), ".none");
}

#[test]
fn tag_keyed_record_displays_with_dotted_keys() {
    let out = common::run(
        "tag_keyed_record",
        "let r = [.dev: 8080, .prod: 443]\necho $r\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[.dev: 8080, .prod: 443]");
}

#[test]
fn list_of_variants_round_trips() {
    let out = common::run(
        "variant_list",
        "echo [.ok 1, .err hello]\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[.ok 1, .err hello]");
}

#[test]
fn mixed_alphabet_record_is_parse_error() {
    let out = common::run(
        "mixed_alphabet",
        "let r = [host: x, .dev: 8080]\n",
    );
    assert_ne!(out.status, 0, "expected failure, got success");
    let combined = format!("{}{}", out.stdout, out.stderr);
    assert!(
        combined.contains("mixes bare and tag keys"),
        "expected mixed-alphabet message in output, got:\n{combined}"
    );
}

#[test]
fn variant_payload_can_be_record() {
    // `.tag` greedily reads the next atom as payload — including a record
    // literal.  Display shows the payload after the tag.
    let out = common::run(
        "variant_with_record_payload",
        "echo .more [head: 1, foo: 2]\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), ".more [head: 1, foo: 2]");
}

// ─── Case (sum eliminator, Phase B) ───────────────────────────────────────────

#[test]
fn case_dispatches_to_ok_arm() {
    let out = common::run(
        "case_ok",
        "let r = .ok 5\nlet x = case $r [.ok: { |x| return $x }, .err: { |_| return -1 }]\necho $x\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "5");
}

#[test]
fn case_dispatches_to_err_arm() {
    let out = common::run(
        "case_err",
        "let r = .err nope\nlet x = case $r [.ok: { |s| return $s }, .err: { |m| return $m }]\necho $x\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "nope");
}

#[test]
fn case_handles_nullary_tag() {
    let out = common::run(
        "case_nullary",
        "let r = .none\nlet x = case $r [.none: { |_| return absent }, .some: { |_| return present }]\necho $x\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "absent");
}

// ─── Step (demand-driven streams, Phase D) ────────────────────────────────────

#[test]
fn step_into_list_finite() {
    let out = common::run(
        "step_finite",
        "let s = !{step-cons 1 { !{step-cons 2 { !{step-cons 3 { !{step-done} } } } } }}\necho !{step-into-list { return $s }}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[1, 2, 3]");
}

#[test]
fn step_take_from_finite_source() {
    let out = common::run(
        "step_take",
        "let s = !{step-cons 1 { !{step-cons 2 { !{step-cons 3 { !{step-cons 4 { !{step-done} } } } } } } }}\necho !{step-into-list { !{step-take 2 { return $s }} }}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[1, 2]");
}

#[test]
fn step_take_terminates_on_infinite_producer() {
    // The canonical demand-driven test: a self-recursive `nats` produces
    // 0, 1, 2, … indefinitely; `step-take 5` cuts the chain short by
    // never forcing the sixth tail thunk.  Phase C's equi-recursive
    // comp types are what let `nats` typecheck; Phase D's combinators
    // make the laziness effective at runtime.
    let out = common::run(
        "step_lazy",
        "let nats = { |n| step-cons $n { !{nats $[$n + 1]} } }\necho !{step-into-list { !{step-take 5 { !{nats 0} } } }}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[0, 1, 2, 3, 4]");
}

#[test]
fn step_fold_sums() {
    let out = common::run(
        "step_fold",
        "let s = !{step-cons 1 { !{step-cons 2 { !{step-cons 3 { !{step-done} } } } } }}\necho !{step-fold { |acc x| return $[$acc + $x] } 0 { return $s }}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "6");
}

#[test]
fn step_map_doubles_each_element() {
    let out = common::run(
        "step_map",
        "let s = !{step-cons 1 { !{step-cons 2 { !{step-cons 3 { !{step-done} } } } } }}\necho !{step-into-list { !{step-map { |x| return $[$x * 2] } { return $s }} }}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[2, 4, 6]");
}

// ─── Step pipeline integration (Phase D.3/4) ──────────────────────────────────

#[test]
fn step_value_drives_pipeline_consumer() {
    // A finite Step value piped into a consumer iterates element-by-element.
    let out = common::run(
        "step_pipe_finite",
        "let s = !{step-cons 1 { !{step-cons 2 { !{step-cons 3 { !{step-done} } } } } }}\nreturn $s | { |x| echo \"got $x\" }\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "got 1\ngot 2\ngot 3");
}

#[test]
fn step_pipeline_terminates_on_infinite_producer_with_take() {
    // The plan's headline pattern: lazy producer + take + pipeline consumer.
    // The consumer's loop is the driver; the producer suspends in unforced
    // tail thunks past the take cut.
    let out = common::run(
        "step_pipe_lazy",
        "let nats = { |n| step-cons $n { !{nats $[$n + 1]} } }\n!{step-take 5 { !{nats 0} } } | { |x| echo $x }\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "0\n1\n2\n3\n4");
}

#[test]
fn polymorphic_recursive_scheme_instantiates_independently() {
    // Two distinct streams (Int and String) compose with the same
    // combinators.  Without per-instantiation fresh comp roots, the
    // first call's element type would leak into the second's.  Phase
    // C's Scheme.comp_ty_vars + comp_ty_bindings ensures each call
    // mints a fresh union-find slot for the cyclic root and the free
    // input root, so these unify independently.
    let out = common::run(
        "polymorphic_step",
        "let nats = { |n| step-cons $n { !{nats $[$n + 1]} } }\n\
         let chars = { |c| step-cons $c { !{chars $c} } }\n\
         echo !{step-into-list { !{step-take 3 { !{nats 0} } } }}\n\
         echo !{step-into-list { !{step-take 3 { !{chars 'x'} } } }}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[0, 1, 2]\n[x, x, x]");
}

#[test]
fn step_source_marks_pipeline_producer() {
    // `step-source { producer }` reads as the head of an infinite-stream
    // pipeline.  It is cosmetically equivalent to `!{producer}` but
    // documents intent at the call site.
    let out = common::run(
        "step_pipe_source",
        "let nats = { |n| step-cons $n { !{nats $[$n + 1]} } }\n!{step-take 3 { step-source { nats 0 } }} | { |x| echo $x }\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "0\n1\n2");
}

#[test]
fn step_pipeline_empty_step_runs_consumer_zero_times() {
    // `.done` short-circuits.  The consumer never sees an element, the
    // pipeline returns Unit.
    let out = common::run(
        "step_pipe_empty",
        "return !{step-done} | { |_x| echo should-not-print }\necho after\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "after");
}

#[test]
fn non_step_variant_is_passed_through_unchanged() {
    // A `.ok 5` variant is *not* a Step; the consumer receives it whole.
    // This locks in the structural-recognition rule: only the precise
    // `.more {head, tail: Thunk}` / `.done` shape triggers iteration.
    let out = common::run(
        "non_step_variant",
        "return .ok 5 | { |v| echo $v }\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), ".ok 5");
}

#[test]
fn from_lines_step_drives_inline_pipeline_consumer() {
    // Canonical Step pipeline form: from-lines produces a Step stream and
    // an inline consumer runs once per line through invoke.rs's adapter.
    let out = common::run(
        "from_lines_inline_consumer",
        "echo \"a\nb\nc\" | from-lines | { |line| echo \"L: $line\" }\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "L: a\nL: b\nL: c");
}
