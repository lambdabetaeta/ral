//! End-to-end tests for variants and tag-keyed records (Phase A).
//!
//! These exercise the parser, elaborator, and runtime.  Pure typing
//! behaviour (e.g. variant inference at an open row) is in
//! `core/tests/typecheck.rs`.

mod common;

#[test]
fn variant_displays_with_payload() {
    let out = common::run("variant_with_payload", "let xv = `ok 42\necho $xv\n");
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "`ok 42");
}

#[test]
fn variant_nullary_displays_as_backtick_label() {
    let out = common::run("variant_nullary", "echo `none\n");
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "`none");
}

#[test]
fn tag_keyed_record_displays_with_backtick_keys() {
    let out = common::run(
        "tag_keyed_record",
        "let res = [`dev: 8080, `prod: 443]\necho $res\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[`dev: 8080, `prod: 443]");
}

#[test]
fn list_of_variants_round_trips() {
    let out = common::run("variant_list", "echo [`ok 1, `err hello]\n");
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[`ok 1, `err hello]");
}

#[test]
fn mixed_alphabet_record_is_parse_error() {
    let out = common::run("mixed_alphabet", "let res = [host: x, `dev: 8080]\n");
    assert_ne!(out.status, 0, "expected failure, got success");
    let combined = format!("{}{}", out.stdout, out.stderr);
    assert!(
        combined.contains("mixes bare and tag keys"),
        "expected mixed-alphabet message in output, got:\n{combined}"
    );
}

#[test]
fn variant_payload_can_be_record() {
    // `` `tag `` greedily reads the next atom as payload — including a record
    // literal.  Display shows the payload after the tag, with map keys in
    // sorted order.
    let out = common::run(
        "variant_with_record_payload",
        "echo `more [head: 1, foo: 2]\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "`more [foo: 2, head: 1]");
}

// ─── Case (sum eliminator, Phase B) ───────────────────────────────────────────

#[test]
fn case_dispatches_to_ok_arm() {
    let out = common::run(
        "case_ok",
        "let res = `ok 5\nlet xv = case $res [`ok: { |x| return $x }, `err: { |_| return -1 }]\necho $xv\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "5");
}

#[test]
fn case_dispatches_to_err_arm() {
    let out = common::run(
        "case_err",
        "let res = `err nope\nlet xv = case $res [`ok: { |s| return $s }, `err: { |m| return $m }]\necho $xv\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "nope");
}

#[test]
fn case_handles_nullary_tag() {
    let out = common::run(
        "case_nullary",
        "let res = `none\nlet xv = case $res [`none: { |_| return absent }, `some: { |_| return present }]\necho $xv\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "absent");
}

// ─── Stream (demand-driven streams, Stream) ────────────────────────────────────

#[test]
fn stream_to_list_finite() {
    let out = common::run(
        "step_finite",
        "let s = !{stream-cons 1 { !{stream-cons 2 { !{stream-cons 3 { !{stream-nil} } } } } }}\necho !{stream-to-list $s}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[1, 2, 3]");
}

#[test]
fn stream_take_from_finite_source() {
    let out = common::run(
        "step_take",
        "let s = !{stream-cons 1 { !{stream-cons 2 { !{stream-cons 3 { !{stream-cons 4 { !{stream-nil} } } } } } } }}\nlet t = !{stream-take 2 $s}\necho !{stream-to-list $t}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[1, 2]");
}

#[test]
fn stream_take_terminates_on_infinite_producer() {
    // The canonical demand-driven test: a self-recursive `nats` produces
    // 0, 1, 2, … indefinitely; `stream-take 5` cuts the chain short by
    // never forcing the sixth tail thunk.  Phase C's equi-recursive
    // comp types are what let `nats` typecheck; Stream's combinators
    // make the laziness effective at runtime.
    let out = common::run(
        "step_lazy",
        "let nats = { |n| stream-cons $n { !{nats $[$n + 1]} } }\nlet t = !{stream-take 5 !{nats 0}}\necho !{stream-to-list $t}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[0, 1, 2, 3, 4]");
}

#[test]
fn stream_fold_sums() {
    let out = common::run(
        "step_fold",
        "let s = !{stream-cons 1 { !{stream-cons 2 { !{stream-cons 3 { !{stream-nil} } } } } }}\necho !{stream-fold { |acc x| return $[$acc + $x] } 0 $s}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "6");
}

#[test]
fn stream_map_doubles_each_element() {
    let out = common::run(
        "step_map",
        "let s = !{stream-cons 1 { !{stream-cons 2 { !{stream-cons 3 { !{stream-nil} } } } } }}\nlet m = !{stream-map { |x| return $[$x * 2] } $s}\necho !{stream-to-list $m}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[2, 4, 6]");
}

// ─── Stream pipeline integration (Stream.3/4) ──────────────────────────────────

#[test]
fn stream_value_passed_to_stream_each_runs_per_element() {
    // Stream elimination is ordinary application, not a value-pipeline edge.
    let out = common::run(
        "step_pipe_finite",
        "let s = !{stream-cons 1 { !{stream-cons 2 { !{stream-cons 3 { !{stream-nil} } } } } }}\nstream-each { |x| echo \"got $x\" } $s\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "got 1\ngot 2\ngot 3");
}

#[test]
fn stream_each_terminates_on_infinite_producer_with_take() {
    // Lazy producer + take + explicit eliminator.  `stream-each` is the
    // driver; the producer suspends in unforced tail thunks past the
    // take cut.
    let out = common::run(
        "step_pipe_lazy",
        "let nats = { |n| stream-cons $n { !{nats $[$n + 1]} } }\nstream-each { |x| echo $x } !{stream-take 5 !{nats 0}}\n",
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
        "let nats = { |n| stream-cons $n { !{nats $[$n + 1]} } }\n\
         let chars = { |c| stream-cons $c { !{chars $c} } }\n\
         let n3 = !{stream-take 3 !{nats 0}}\n\
         echo !{stream-to-list $n3}\n\
         let c3 = !{stream-take 3 !{chars 'x'}}\n\
         echo !{stream-to-list $c3}\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[0, 1, 2]\n[x, x, x]");
}

#[test]
fn stream_each_on_empty_stream_runs_body_zero_times() {
    // `done short-circuits inside `stream-each`.  The body never sees an
    // element.
    let out = common::run(
        "step_pipe_empty",
        "stream-each { |_x| echo should-not-print } !{stream-nil}\necho after\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "after");
}

#[test]
fn variant_value_pipeline_is_rejected() {
    // A variant is a value and cannot cross a byte-pipeline edge.
    let out = common::run("non_step_variant", "return `ok 5 | { |v| echo $v }\n");
    assert_ne!(out.status, 0, "expected a compile error");
    assert!(
        out.stdout.is_empty(),
        "rejected pipeline wrote stdout: {}",
        out.stdout
    );
    assert!(
        out.stderr.contains("this stage produces a value"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn done_labelled_variant_pipeline_is_rejected() {
    let out = common::run("done_payload_variant", "return `done 5 | { |v| echo $v }\n");
    assert_ne!(out.status, 0, "expected a compile error");
    assert!(
        out.stdout.is_empty(),
        "rejected pipeline wrote stdout: {}",
        out.stdout
    );
    assert!(
        out.stderr.contains("this stage produces a value"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn from_lines_stream_consumed_by_stream_each() {
    // `from-lines` is a decoder tail; consume its returned stream with an
    // ordinary application.
    let out = common::run(
        "from_lines_inline_consumer",
        "let lines = !{echo \"a\nb\nc\" | from-lines}\nstream-each { |line| echo \"L: $line\" } $lines\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "L: a\nL: b\nL: c");
}
