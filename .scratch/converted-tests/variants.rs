//! End-to-end tests for variants (Phase A).
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
fn list_of_variants_round_trips() {
    let out = common::run("variant_list", "echo [`ok 1, `err hello]\n");
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[`ok 1, `err hello]");
}

#[test]
fn tag_key_is_a_parse_error() {
    let out = common::run("tag_key", "let res = [`dev: 8080, `prod: 443]\n");
    assert_ne!(out.status, 0, "expected failure, got success");
    let combined = format!("{}{}", out.stdout, out.stderr);
    assert!(
        combined.contains("names a variant"),
        "expected the tag-is-not-a-key message in output, got:\n{combined}"
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

/// An arm naming a handler binds the payload as surely as an inline one, and
/// its bytes belong to the binding, not the terminal: the arm elaborates to
/// that handler applied to the payload, so the coercion lands inside it.
/// Without it the bytes would reach the terminal and the binding would be
/// empty.
#[test]
fn case_arm_naming_a_handler_is_captured_like_an_inline_one() {
    let named = common::run(
        "case_named_arm",
        "let h = { |p| echo b }\n\
         let xv = case `some () [`some: $h, `none: { |p| echo z }]\n\
         echo \"got [$xv]\"\n",
    );
    assert_eq!(named.status, 0, "stderr: {}", named.stderr);
    assert_eq!(named.stdout.trim(), "got [b]");

    let inline = common::run(
        "case_inline_arm",
        "let h = { |p| echo b }\n\
         let xv = case `some () [`some: { |p| $h $p }, `none: { |p| echo z }]\n\
         echo \"got [$xv]\"\n",
    );
    assert_eq!(inline.status, 0, "stderr: {}", inline.stderr);
    assert_eq!(
        inline.stdout, named.stdout,
        "the two spellings of one arm must run alike"
    );
}

/// An arm is a branch, not a function the runtime applies: it runs in the
/// ambient control context rather than a frame of its own.  A script that
/// returns exits 0 whatever it returned — a `Bool` is data, not a verdict —
/// so a `case` whose last arm answers `false` still exits clean.
#[test]
fn case_arm_returning_a_bool_exits_zero() {
    let out = common::run(
        "case_arm_leaves_status",
        "true\n\
         case `go () [`go: { |_| false }]\n",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
}

// ─── User-built lazy lists: recursive variants with thunked tails ─────────────

/// A lazy list whose tail is a thunk, so a producer runs only as far as
/// demand reaches.  Its type is equi-recursive, closing through the thunk.
const LAZY_LIST: &str = "\
let cons = { |head tail| return `more [head: $head, tail: $tail] }
let take-lazy = { |n s|
    if $[$n <= 0] { return `done } else {
        case $s [
            `more: { |p| cons $p[head] { !{take-lazy $[$n - 1] !$p[tail]} } },
            `done: { |_| return `done }
        ]
    }
}
let to-list = { |s|
    case $s [
        `more: { |p|
            let rest = !{to-list !$p[tail]}
            return [$p[head], ...$rest]
        },
        `done: { |_| return [] }
    ]
}
";

#[test]
fn take_terminates_on_an_infinite_producer() {
    // `nats` never ends; `take-lazy 5` never forces the sixth tail thunk.
    let script = [
        LAZY_LIST,
        "let nats = { |n| cons $n { !{nats $[$n + 1]} } }\n\
         echo !{to-list !{take-lazy 5 !{nats 0}}}\n",
    ]
    .concat();
    let out = common::run("lazy_take", &script);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[0, 1, 2, 3, 4]");
}

#[test]
fn polymorphic_recursive_scheme_instantiates_independently() {
    // An Int producer and a String one share the combinators: each call mints
    // fresh comp roots, so the first element type cannot leak into the second.
    let script = [
        LAZY_LIST,
        "let nats = { |n| cons $n { !{nats $[$n + 1]} } }\n\
         let chars = { |c| cons $c { !{chars $c} } }\n\
         echo !{to-list !{take-lazy 3 !{nats 0}}}\n\
         echo !{to-list !{take-lazy 3 !{chars 'x'}}}\n",
    ]
    .concat();
    let out = common::run("lazy_polymorphic", &script);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "[0, 1, 2]\n[x, x, x]");
}

// ─── Variants at a pipeline edge ──────────────────────────────────────────────

#[test]
fn a_variant_piped_into_a_block_goes_nowhere() {
    // A variant is a value, and no value crosses an edge: the producer's
    // `ok 5` is discarded, the block literal is returned rather than
    // applied, and so `$v` never prints. `return ()` keeps the run's result
    // data.
    let out = common::run(
        "non_step_variant",
        "return `ok 5 | { |v| echo $v }\nreturn ()\n",
    );
    assert_eq!(out.status, 0, "expected acceptance: {}", out.stderr);
    assert!(
        out.stdout.is_empty(),
        "a returned thunk must never run: {}",
        out.stdout
    );
}

/// The same, for a `done`-labelled variant: no label earns special treatment
/// at an edge, because no value reaches one.
#[test]
fn a_done_labelled_variant_piped_into_a_block_goes_nowhere() {
    let out = common::run(
        "done_payload_variant",
        "return `done 5 | { |v| echo $v }\nreturn ()\n",
    );
    assert_eq!(out.status, 0, "expected acceptance: {}", out.stderr);
    assert!(
        out.stdout.is_empty(),
        "a returned thunk must never run: {}",
        out.stdout
    );
}
