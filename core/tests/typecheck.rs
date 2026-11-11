//! Behavioural oracle for the HM static type checker: what the checker
//! computes and annotates, read off the checker's own data.
//!
//! Every test parses + elaborates a small ral program and runs `typecheck()`.
//! Capture and grade refusals and hints (SPEC §17.6) are checked here too;
//! the broader corpus lives under `tests/reject/` and `tests/accept/`, run by
//! `ral/tests/corpus.rs`.

#![allow(clippy::disallowed_methods, reason = "test code")]

mod common;

use ral_core::fact::RefusedGrantKey;
use ral_core::ir::{Comp, CompKind, GroupNode, Phrase, Toplevel, Unchecked, Val};
use ral_core::ty::{CompTyNode, Grade, Ty, TyNode};
use ral_core::typecheck::contract::declared;
use ral_core::typecheck::{Form, TypeErrorKind};
use ral_core::{TypeError, elaborator::elaborate, syntax::parser::parse, typecheck};
use std::sync::Arc;

fn raw_errors(src: &str) -> Vec<TypeError> {
    errors_against(src, &ral_core::HostSurface::default())
}

/// `src` as a session dressed with `surface` sees it.  Almost everything is
/// checked against the bare core table `raw_errors` passes; a name only a host
/// installs must be checked against a surface that carries it, or the checker
/// reads it as an external and answers about something else.
fn errors_against(src: &str, surface: &ral_core::HostSurface) -> Vec<TypeError> {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    let comp =
        elaborate(&ast, [], None).unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
    typecheck(&comp, common::schemes_for(surface), None)
        .err()
        .unwrap_or_default()
}

/// A surface holding `detach`, the base frame core publishes for a host to install.
#[cfg(unix)]
fn detach_surface() -> ral_core::HostSurface {
    ral_core::HostSurface {
        statics: vec![ral_core::shell::builtins::DETACH_BUILTIN],
        ..Default::default()
    }
}

fn errors(src: &str) -> Vec<String> {
    raw_errors(src)
        .into_iter()
        .map(|e| e.kind.render_message())
        .collect()
}

/// `src` checks cleanly against a session dressed with `surface`.
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

/// `workers` is data: a row's fields read at their documented types, including
/// the three-state `state` variant (a consumed worker is never listed).
#[test]
fn workers_lists_rows_at_their_documented_types() {
    let errs = errors(
        "let w = workers\n\
         let id = $[$w[0][id] + 1]\n\
         let t = string-replace 'x' 'y' $w[0][title]\n\
         let e = $[$w[0][elapsed] + 1.0]\n\
         let s = case $w[0][state] [\n\
             `running: { |_| return 'run' },\n\
             `stopping: { |_| return 'stop' },\n\
             `ready: { |_| return 'ready' }\n\
         ]",
    );
    assert!(
        errs.is_empty(),
        "a `workers` row must read at its documented types, got: {errs:?}"
    );
}

/// `poll` returns the four-arm lifecycle variant (`running`, `stopping`,
/// `ready`, `consumed`), and a two-arm `pending`/`settled` case is rejected.
#[test]
fn poll_requires_four_lifecycle_arms_and_rejects_old_two_arm_tags() {
    let errs = errors(
        "let h = spawn { return 1 }\n\
         let _ = !{is-done $h}\n\
         let n = case !{poll $h} [\n\
             `running: { |r| let _ = $[$r[elapsed] + 1.0]; return !{length $r[stdout]} },\n\
             `stopping: { |s| let _ = $[$s[elapsed] + 1.0]; return -1 },\n\
             `ready: { |d| let _ = $[$d[elapsed] + 1.0]; case $d[outcome] [`ok: { |v| return $v }, `err: { |_| return -2 }] },\n\
             `consumed: { |c| let _ = $[$c[elapsed] + 1.0]; return -3 }\n\
         ]",
    );
    assert!(
        errs.is_empty(),
        "four-arm `poll` must typecheck cleanly, got: {errs:?}"
    );

    let old_two_arm = "let h = spawn { return 1 }\n\
                       case !{poll $h} [`settled: { |_| return 1 }, `pending: { |_| return 0 }]";
    assert_eq!(codes_of(&raw_errors(old_two_arm)), ["T0030"]);

    let missing_stopping = "let h = spawn { return 1 }\n\
                            case !{poll $h} [\n\
                                `running: { |_| return 0 },\n\
                                `ready: { |_| return 1 },\n\
                                `consumed: { |_| return 2 }\n\
                            ]";
    assert_eq!(codes_of(&raw_errors(missing_stopping)), ["T0030"]);
}

/// `await` produces what its handle's block produces: a command block's
/// handle awaits as a command, whose bind captures its output, and a value
/// block's as its value.
#[test]
fn await_has_its_handles_grade() {
    let top = toplevel_ok(
        "let h = spawn { echo hi }\n\
         let c = { await $h }\n\
         let x = await $h\n\
         return ()",
    );
    assert_eq!(scheme_at(&top, 0), "Handle Command");
    assert_eq!(scheme_at(&top, 1), "Command");
    assert_eq!(scheme_at(&top, 2), "String");
    let top = toplevel_ok("let h = spawn { 1 }\nlet n = await $h\nreturn ()");
    assert_eq!(scheme_at(&top, 0), "Handle (Returns Integer)");
    assert_eq!(scheme_at(&top, 1), "Integer");
}

/// `spawn`'s handle carries its body's producer, titled or not, and its
/// title is a String.
#[test]
fn spawn_is_typed_by_its_body() {
    let top = toplevel_ok("let a = spawn { echo hi }\nlet b = spawn \"t\" { 1 }\nreturn ()");
    assert_eq!(scheme_at(&top, 0), "Handle Command");
    assert_eq!(scheme_at(&top, 1), "Handle (Returns Integer)");
    let errs = raw_errors("spawn 1 { 1 }");
    assert!(
        errs.iter().any(|e| e
            .hint()
            .is_some_and(|h| h.contains("a spawn's title is a String"))),
        "expected a title mismatch, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

/// `gather` passes its body's type through, grade and all.
#[test]
fn gather_passes_its_bodys_type() {
    let top = toplevel_ok("let c = { gather { echo hi } }\nlet n = { gather { 1 } }\nreturn ()");
    assert_eq!(scheme_at(&top, 0), "Command");
    assert_eq!(scheme_at(&top, 1), "Returns Integer");
}

/// `par` command callbacks replay output;
/// the list holds their unit values, not captured strings.
#[test]
fn par_accepts_a_command_callback_and_returns_its_unit_values() {
    let src = "let xs = par { |x| echo $x } [1, 2] 2\nreturn ()";
    let top = toplevel_ok(src);
    assert_eq!(scheme_at(&top, 0), "[Unit]");
    assert_eq!(captures(src), 0);

    let repaired = "let xs = par { |x| return !{echo $x} } [1, 2] 2\nreturn ()";
    assert_eq!(scheme_at(&toplevel_ok(repaired), 0), "[String]");
    assert_eq!(captures(repaired), 1);
}

/// A base frame takes an argv, and `...` is exactly its notation.  `detach` is
/// checked against a surface that installs it, because core's table alone
/// publishes only `echo`: read as an external, `detach` would accept the spread
/// for an external's reasons and say nothing about frames.
#[cfg(unix)]
#[test]
fn a_spread_into_detach_is_legal() {
    ok_against(
        "let xs = ['300']; detach #'a long sleep'# /bin/sleep ...$xs",
        &detach_surface(),
    );
}

/// `detach` is a frame for the same reason `echo` is: it takes an argv, so
/// there is no `$detach` either.  Checked against the surface that installs it
/// — the bare table would answer about an external.
#[cfg(unix)]
#[test]
fn detach_is_a_base_frame_not_a_value() {
    let codes: Vec<_> = errors_against("return $detach", &detach_surface())
        .iter()
        .map(|e| e.kind.code())
        .collect();
    assert_eq!(codes, ["T0042"]);
}

/// An arm for `detach` stands in for ral's own `detach`, so it returns the
/// receipt `detach` returns — not a command's `()`, and not anything else.
#[cfg(unix)]
#[test]
fn an_arm_for_detach_returns_its_receipt() {
    ok_against(
        "within [handlers: [detach: { |a| return [pid: 1, desc: 'stub'] }]] { return () }",
        &detach_surface(),
    );
    let codes: Vec<_> = errors_against(
        "within [handlers: [detach: { |a| return 3 }]] { return () }",
        &detach_surface(),
    )
    .iter()
    .map(|e| e.kind.code())
    .collect();
    assert_eq!(codes, ["T0011"]);
}

// ─── Toplevel phrase typechecking ─────────────────────────────────────────

fn toplevel(src: &str) -> Unchecked {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    elaborate(&ast, [], None).unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"))
}

fn toplevel_ok(src: &str) -> Toplevel {
    typecheck(
        &toplevel(src),
        common::schemes_for(&ral_core::HostSurface::default()),
        None,
    )
    .unwrap_or_else(|errs| {
        let msgs: Vec<String> = errs.iter().map(|e| e.kind.render_message()).collect();
        panic!("expected no errors in {src:?}, got: {msgs:?}")
    })
}

/// A two-member recursive group generalises each member on its own type:
/// the n-ary `Rec` types each member independently, not through one shared
/// `Map` shape.
#[test]
fn toplevel_rec_group_members_generalise_independently() {
    let top = toplevel_ok(
        "let f = { |x| let _ = $[$x + 1]; let _ = !{g \"s\"}; return true }\n\
         let g = { |y| let _ = !{upper $y}; let _ = !{f 1}; return 1 }\n\
         return ()",
    );
    assert_eq!(scheme_at(&top, 0), "Integer → Returns Bool");
    assert_eq!(scheme_at(&top, 1), "String → Returns Integer");
}

#[test]
fn checked_mutual_recursion_projections_share_one_group_arc() {
    let top = toplevel_ok(
        "let f = { |x| let _ = $[$x + 1]; let _ = !{g \"s\"}; return true }\n\
         let g = { |y| let _ = !{upper $y}; let _ = !{f 1}; return 1 }\n\
         return ()",
    );
    fn projection(phrase: &Phrase) -> (&Arc<GroupNode>, usize) {
        let Phrase::Define { comp, .. } = phrase else {
            panic!("expected a Define, got {phrase:?}");
        };
        let CompKind::Return(Val::Thunk(rec)) = &comp.item else {
            panic!("expected Return(Thunk(Rec)), got {:?}", comp.item);
        };
        let CompKind::Rec { group, index } = &rec.shape().item else {
            panic!("expected a Rec node, got {:?}", rec.shape().item);
        };
        (group, *index)
    }
    let (f_group, f_index) = projection(&top.phrases[0].item);
    let (g_group, g_index) = projection(&top.phrases[1].item);
    assert_eq!((f_index, g_index), (0, 1));
    assert!(
        Arc::ptr_eq(f_group, g_group),
        "checked projections must share one group Arc"
    );
    assert_eq!(f_group.shape().len(), 2);
    assert_eq!(f_group.shape()[0].0.as_ref(), "f");
    assert_eq!(f_group.shape()[1].0.as_ref(), "g");
}

/// The independent dependencies of one binding are checked in source order,
/// so the first error is the earliest, not whichever the hasher puts first.
#[test]
fn independent_dependencies_are_checked_in_source_order() {
    let src = [
        "let a = { b; c; d; e }",
        "let b = { return $[1 + 'x'] }",
        "let c = { return $[2 + 'y'] }",
        "let d = { return $[3 + 'z'] }",
        "let e = { return $[4 + 'w'] }",
        "a",
    ]
    .join("\n");
    // Hash order varies per run; one pass would succeed by luck.
    for _ in 0..16 {
        let at = raw_errors(&src)[0].pos.expect("a located error").start as usize;
        let line = src[..at].matches('\n').count() + 1;
        assert_eq!(line, 2, "the first error is `b`'s, on line 2");
    }
}

#[test]
fn pattern_siblings_constrain_each_other_before_generalisation() {
    for pattern in ["[a, [b]]", "[[b], a]"] {
        let src = format!("let {pattern} = []\nlet c = $[$a + 1]");
        assert_eq!(codes_of(&raw_errors(&src)), ["T0074"], "{src}");
    }
}

#[test]
fn both_names_in_a_list_pattern_remain_polymorphic() {
    let top = toplevel_ok(
        "let id = { |x| return $x }\n\
         let [f, g] = [$id, $id]\n\
         let fi = f 1\n\
         let fs = f 's'\n\
         let gi = g 2\n\
         let gs = g 't'\n\
         return ()",
    );
    for (index, expected) in [(2, "Integer"), (3, "String"), (4, "Integer"), (5, "String")] {
        assert_eq!(scheme_at(&top, index), expected);
    }
}

/// A right-hand side of function type is bound unrun, as its thunk (Let-fun),
/// at any depth: `let g = f 1` is `Return(Thunk(App))`, generalised, and so is
/// a `let` inside a lambda's body.
#[test]
fn a_function_typed_rhs_is_bound_as_its_thunk_at_any_depth() {
    let top = toplevel_ok(
        "let f = { |a b| return $a }\n\
         let g = f 1\n\
         let h = { |n| let k = f 2; k $n }\n\
         return ()",
    );
    let Phrase::Define { comp, schemes, .. } = &top.phrases[1].item else {
        panic!("expected g's Define, got {:?}", top.phrases[1].item);
    };
    let CompKind::Return(Val::Thunk(g)) = &comp.item else {
        panic!("expected Return(Thunk(..)), got {:?}", comp.item);
    };
    assert!(
        matches!(g.shape().item, CompKind::App { .. }),
        "g binds the call itself, unrun: {:?}",
        g.shape().item
    );
    let rendered = schemes[0].1.to_string();
    assert!(
        rendered.starts_with('∀') && rendered.contains('→'),
        "expected g : U (B → C), got: {rendered}"
    );
    let Phrase::Define { comp, .. } = &top.phrases[2].item else {
        panic!("expected h's Define, got {:?}", top.phrases[2].item);
    };
    let CompKind::Return(Val::Thunk(h)) = &comp.item else {
        panic!("expected Return(Thunk(..)), got {:?}", comp.item);
    };
    let CompKind::Lam { body, .. } = &h.shape().item else {
        panic!("expected h's Lam, got {:?}", h.shape().item);
    };
    let CompKind::Bind { comp: k, .. } = &body.item else {
        panic!("expected k's Bind, got {:?}", body.item);
    };
    assert!(
        matches!(k.item, CompKind::Return(Val::Thunk(_))),
        "an inner let of function type is thunked too: {:?}",
        k.item
    );
}

// ─── Reads: the key form determines the container ─────────────────────────

/// The scheme the `let` at phrase `index` closed with, as `explain` prints it.
fn scheme_at(top: &Toplevel, index: usize) -> String {
    let Phrase::Define { schemes, .. } = &top.phrases[index].item else {
        panic!(
            "expected a Define at {index}, got {:?}",
            top.phrases[index].item
        );
    };
    schemes[0].1.to_string()
}

#[test]
fn a_label_read_requires_a_record_and_refuses_a_computed_key() {
    let src = "let f = { |m k| let a = $m[x]; return $m[$k] }\nreturn ()";
    let errs = raw_errors(src);
    assert_eq!(codes_of(&errs), ["T0010"]);
    assert!(errs[0].hint().unwrap().contains("write the name"));
}

#[test]
fn a_computed_key_helper_generalises_over_map_element_types() {
    let top = toplevel_ok(
        "let pick = { |m k| $m[$k] }\n\
         let a = pick [:, x: 1] 'x'\n\
         let b = pick [:, y: true] 'y'\n\
         return ()",
    );
    // α admits unit, so its fresh grade stays visible (SPEC §17.3).
    assert_eq!(scheme_at(&top, 0), "∀α ν. Map α → String → Command[ν] α");
    assert_eq!(scheme_at(&top, 1), "Integer");
    assert_eq!(scheme_at(&top, 2), "Bool");
}

#[test]
fn a_written_position_is_the_list_rule_and_stays_generic() {
    let top = toplevel_ok(
        "let first = { |xs| $xs[0] }\n\
         let a = first [1, 2]\n\
         let b = first ['x']\n\
         return ()",
    );
    // The generic read keeps its fresh grade (SPEC §17.3).
    assert_eq!(scheme_at(&top, 0), "∀α ν. [α] → Command[ν] α");
}

#[test]
fn a_computed_list_position_is_refused_with_the_nth_help() {
    let errs = raw_errors("let xs = [1, 2]\nlet i = 0\nreturn $xs[$i]");
    assert_eq!(codes_of(&errs), ["T0010", "T0010"]);
    assert!(
        errs.iter()
            .all(|e| e.hint().unwrap().contains("nth $i $xs"))
    );
}

#[test]
fn nth_helpers_generalise_over_list_element_types() {
    for body in ["nth $i $xs", "let j = $[$i + 1]; nth $j $xs"] {
        let top = toplevel_ok(&format!(
            "let pick = {{ |xs i| {body} }}\n\
             let a = pick [1, 2] 0\n\
             let b = pick ['x', 'y'] 0\n\
             return ()"
        ));
        // The result's α admits unit (SPEC §17.3).
        assert_eq!(scheme_at(&top, 0), "∀α ν. [α] → Integer → Command[ν] α");
        assert_eq!(scheme_at(&top, 1), "Integer");
        assert_eq!(scheme_at(&top, 2), "String");
    }
}

/// Walking a value of unknown shape by computed keys makes it a map of
/// itself.
#[test]
fn a_walk_down_a_decoded_value_is_a_map_of_itself() {
    let top = toplevel_ok(
        "let walk = { |doc keys| fold { |node key| return $node[$key] } $doc $keys }\n\
         let leaf = walk [:] ['a']\n\
         return ()",
    );
    assert_eq!(
        scheme_at(&top, 0),
        "μα. Map α → [String] → Returns μα. Map α"
    );
}

#[test]
fn a_numeric_computed_key_is_refused_with_the_nth_help() {
    let errs = raw_errors("let f = { |xs k| let j = $[$k + 1]; $xs[$k] }\nreturn ()");
    assert_eq!(codes_of(&errs), ["T0010"]);
    assert!(errs[0].hint().unwrap().contains("nth $i $xs"));
}

#[test]
fn a_target_of_comparable_kind_is_refused_with_the_kind_sentence() {
    let src = "let f = { |c d k| let b = $[$c < $d]; $c[$k] }\nreturn ()";
    assert_eq!(codes_of(&raw_errors(src)), ["T0074"]);
    has_error(src, "this is a map, but a number or text is needed");
}

#[test]
fn a_record_pattern_on_an_integer_list_reports_the_known_element_type() {
    has_error("let [a: x] = [1, 2]", "couldn't match type [Integer]");
}

#[test]
fn a_bare_block_condition_reports_its_known_boolean_result() {
    has_error(
        "if { return $[1 == 1] } { echo yes }",
        "couldn't match type {Returns Bool} with type Bool",
    );
}

#[test]
fn a_non_function_case_arm_reports_its_known_payload_and_unknown_result() {
    let errs = raw_errors("case `a () [`a: 7]");
    assert_eq!(codes_of(&errs), ["T0010"]);
    let TypeErrorKind::TyMismatch { expected, actual } = &errs[0].kind else {
        panic!("expected a value type mismatch: {errs:?}")
    };
    assert_eq!(expected, &Ty::int());
    let TyNode::Thunk(body) = actual.node() else {
        panic!("expected a handler thunk: {errs:?}")
    };
    assert!(
        matches!(body.node(), CompTyNode::Fun(arg, result)
            if *arg == Ty::unit() && matches!(result.node(), CompTyNode::Var(_))),
        "coverage fixes the payload but not the result: {errs:?}"
    );
}

#[test]
fn a_boolean_computed_key_is_refused_as_a_non_string() {
    let src = "let k = $[1 < 2]\nlet f = { |m| $m[$k] }\nreturn ()";
    let errs = raw_errors(src);
    assert_eq!(codes_of(&errs), ["T0010"]);
    assert_eq!(
        errs[0].kind.render_message(),
        "couldn't match type Bool with type String"
    );
}

// ─── The return contract: a contract file's returned row ─────────────────────

/// A host's own table, held to the same condition the declared four are.
static TEST_TABLE: ral_core::typecheck::Table = ral_core::typecheck::Table {
    form: "test",
    keys: &[
        ral_core::typecheck::contract::Key {
            label: "n",
            holds: ral_core::typecheck::contract::Holds::At(Ty::int),
            reason: None,
        },
        ral_core::typecheck::contract::Key {
            label: "loose",
            holds: ral_core::typecheck::contract::Holds::Decoded,
            reason: None,
        },
    ],
    factory: false,
};

/// A table with a key the row must have and a key it knows and refuses — the
/// manifest's `name:` and `capabilities:` in miniature.
static REQUIRED_TABLE: ral_core::typecheck::Table = ral_core::typecheck::Table {
    form: "required",
    keys: &[
        ral_core::typecheck::contract::Key {
            label: "must",
            holds: ral_core::typecheck::contract::Holds::Required(Ty::string),
            reason: None,
        },
        ral_core::typecheck::contract::Key {
            label: "loose",
            holds: ral_core::typecheck::contract::Holds::Decoded,
            reason: None,
        },
        ral_core::typecheck::contract::Key {
            label: "capabilities",
            holds: ral_core::typecheck::contract::Holds::Refused(
                RefusedGrantKey::ManifestCapabilitiesUnenforced,
            ),
            reason: None,
        },
    ],
    factory: false,
};

fn contract_errors(table: &'static ral_core::typecheck::Table, src: &str) -> Vec<TypeError> {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    let top =
        elaborate(&ast, [], None).unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
    typecheck(
        &top,
        common::schemes_for(&ral_core::HostSurface::default()),
        Some(table),
    )
    .err()
    .unwrap_or_default()
}

fn codes_of(errs: &[TypeError]) -> Vec<&'static str> {
    errs.iter().map(|e| e.kind.code()).collect()
}

fn one_error_with_hint(src: &str, code: &str, hint: &str) -> TypeError {
    let mut errs = raw_errors(src);
    assert_eq!(codes_of(&errs), [code], "{src}: {errs:?}");
    let err = errs.remove(0);
    assert_eq!(err.hint().as_deref(), Some(hint), "{src}: {err:?}");
    err
}

fn assert_error_in(src: &str, err: &TypeError, fragment: &str) {
    let start = src
        .rfind(fragment)
        .expect("the source contains the blamed result");
    let span = err.pos.expect("a located error");
    assert!(
        span.start as usize >= start && span.end as usize <= start + fragment.len(),
        "expected the error in {fragment:?}: {src}: {err:?}"
    );
}

fn schema_errors(src: &str) -> Vec<TypeError> {
    contract_errors(&TEST_TABLE, src)
}

/// A key the row must have is demanded, and its absence is not an unknown
/// key — the plugin manifest's `name:` is this shape.
#[test]
fn return_schema_demands_a_required_key() {
    let errs = contract_errors(&REQUIRED_TABLE, "return [loose: 1]");
    assert!(
        errs.iter().any(|e| e.kind.code() == "T0021"),
        "expected T0021, got: {errs:?}"
    );
    assert!(
        contract_errors(&REQUIRED_TABLE, "return [must: 'here']").is_empty(),
        "the required key, written, satisfies it"
    );
}

#[test]
fn return_schema_catches_a_wrong_typed_literal_field() {
    let errs = schema_errors("return [n: \"x\"]");
    assert_eq!(codes_of(&errs), ["T0010"], "{errs:?}");
}

#[test]
fn return_schema_accepts_a_correctly_typed_literal_field() {
    let errs = schema_errors("return [n: 1]");
    assert!(errs.is_empty(), "expected no schema errors, got: {errs:?}");
}

/// The row is what is checked, not the syntax that built it: a return the
/// program computed is held exactly as one written out is.
#[test]
fn return_schema_checks_a_computed_return_value() {
    let errs = schema_errors("let m = [n: \"x\"]\nreturn $m");
    assert_eq!(codes_of(&errs), ["T0010"], "{errs:?}");
}

/// The bad key arrives through a spread, which a literal-only rule would wave
/// through.
#[test]
fn return_schema_catches_a_key_misspelled_behind_a_spread() {
    let errs = schema_errors("let extra = [nn: 1, loose: 0]\nreturn [...$extra, loose: 1]");
    assert!(
        errs.iter().any(|e| e.kind.code() == "T0076"),
        "expected T0076, got: {errs:?}"
    );
}

/// A known refused key preserves its intrinsic advice rather than becoming
/// an unknown key.
#[test]
fn return_schema_keeps_a_refused_keys_own_sentence() {
    let errs = contract_errors(&REQUIRED_TABLE, "return [must: 'here', capabilities: 1]");
    let refusal = errs
        .iter()
        .find(|e| e.kind.code() == "T0026")
        .unwrap_or_else(|| panic!("expected T0026, got: {errs:?}"));
    assert_eq!(
        refusal.hint().as_deref(),
        Some(RefusedGrantKey::ManifestCapabilitiesUnenforced.advice()),
        "the refusal preserves its intrinsic advice"
    );
    assert_eq!(errs.len(), 1, "and it is not also an unknown key: {errs:?}");
}

/// An unknown key shares the table's facts with the runtime checks.
#[test]
fn return_schema_shares_the_tables_unknown_key_facts() {
    let errs = schema_errors("return [nn: 1]");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].kind.code(), "T0076");
    let TypeErrorKind::UnknownKey { form, key, offered } = &errs[0].kind else {
        panic!("expected unknown-key facts: {errs:?}");
    };
    let expected = TEST_TABLE.unknown_key("nn");
    assert_eq!(*form, TEST_TABLE.form);
    assert_eq!(key, &expected.field);
    assert_eq!(
        offered
            .iter()
            .copied()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        expected.offered
    );
}

/// A map has keys where the table has labels, so the spelling is refused.
#[test]
fn return_schema_refuses_a_map() {
    for form in [Form::Rc, Form::Grant, Form::Manifest] {
        let errs = contract_errors(declared(form), "return [:, nn: 1]");
        assert_eq!(codes_of(&errs), ["T0077"], "{form:?}: {errs:?}");
        let hint = errs[0].hint().unwrap();
        assert!(hint.contains("not a map"), "{hint}");
    }
}

/// `[]` is the empty list; `()` is how a file says it has nothing to set.
#[test]
fn return_schema_refuses_a_list_and_points_at_unit() {
    for form in [Form::Rc, Form::Grant] {
        let errs = contract_errors(declared(form), "return []");
        assert_eq!(codes_of(&errs), ["T0077"], "{form:?}: {errs:?}");
        assert!(errs[0].hint().unwrap().contains("returns `()`"));
    }
}

#[test]
fn return_schema_refuses_a_function_unless_the_table_admits_a_factory() {
    let factory = "return { |options| [name: 'p'] }";
    for form in [Form::Rc, Form::Grant] {
        let errs = contract_errors(declared(form), factory);
        assert_eq!(codes_of(&errs), ["T0077"], "{form:?}: {errs:?}");
    }
    assert!(contract_errors(declared(Form::Manifest), factory).is_empty());
}

/// A factory is a block of one parameter, the options, making the manifest:
/// held to that shape statically, and what it makes ascribed as a direct
/// return is.
#[test]
fn a_returned_block_is_held_to_the_factory_shape() {
    for src in ["return { [name: 'p'] }", "return { |a b| [name: 'p'] }"] {
        let errs = contract_errors(declared(Form::Manifest), src);
        assert_eq!(codes_of(&errs), ["T0011"], "{src}: {errs:?}");
    }
    let errs = contract_errors(declared(Form::Manifest), "return { |options| [nam: 'p'] }");
    assert_eq!(codes_of(&errs), ["T0076", "T0021"], "{errs:?}");
    let made = "let mk = { |n options| return [name: $n] }\nlet f = mk 'p'\nreturn $f";
    assert!(contract_errors(declared(Form::Manifest), made).is_empty());
}

#[test]
fn return_schema_refuses_a_scalar() {
    let errs = contract_errors(declared(Form::Rc), "return 3");
    assert_eq!(codes_of(&errs), ["T0077"], "{errs:?}");
}

/// `()` is the empty keyset: fine for a table with nothing required, and a
/// missing `name` for the manifest.
#[test]
fn return_schema_reads_unit_as_nothing_to_set() {
    assert!(contract_errors(declared(Form::Rc), "return ()").is_empty());
    assert!(contract_errors(declared(Form::Grant), "return ()").is_empty());
    let errs = contract_errors(declared(Form::Manifest), "return ()");
    assert_eq!(codes_of(&errs), ["T0021"], "{errs:?}");
}

/// A file with no returning phrase evaluates to `()`, so it is ascribed as
/// `return ()` is.
#[test]
fn return_schema_reads_a_file_with_no_returning_phrase_as_unit() {
    for src in ["", "let name = 'p'"] {
        assert!(
            contract_errors(declared(Form::Rc), src).is_empty(),
            "{src:?}"
        );
        let errs = contract_errors(declared(Form::Manifest), src);
        assert_eq!(codes_of(&errs), ["T0021"], "{src:?}: {errs:?}");
    }
}

/// A return typed at a variable is the runtime check's.
#[test]
fn return_schema_leaves_a_decoded_value_to_the_runtime_check() {
    assert!(contract_errors(declared(Form::Rc), "return !{from-json}").is_empty());
}

/// Ascription reads a finished result, so the order of independent statements
/// decides nothing.
#[test]
fn return_schema_gives_one_verdict_in_either_statement_order() {
    let one = "let a = [surfase: 1]\nlet b = [bell: 2]\nreturn [...$a, ...$b]";
    let two = "let b = [bell: 2]\nlet a = [surfase: 1]\nreturn [...$a, ...$b]";
    let render = |src| {
        let mut msgs: Vec<_> = contract_errors(declared(Form::Rc), src)
            .iter()
            .map(|e| e.kind.render_message())
            .collect();
        msgs.sort();
        msgs
    };
    assert_eq!(render(one).len(), 2);
    assert_eq!(render(one), render(two));
}

/// `$x` is bound by an earlier top-level `let` — a separate `Phrase::Define`
/// from the final `return`'s own phrase — and the contract must resolve it
/// too, not misreport it as unbound.
#[test]
fn return_schema_resolves_an_earlier_top_level_let() {
    let errs = schema_errors("let x = 1\nreturn [n: $x]");
    assert!(
        errs.is_empty(),
        "an earlier top-level let must resolve, got: {errs:?}"
    );
}

// ─── grant tables ─────────────────────────────────────────────────────────────
//
// `grant` holds `net`, `gpu`, `editor` and `shell` to closed variants where they are
// written, and queues `exec` and `fs` to be ascribed against their nested
// tables once inference has finished, so the verdict is the same inline,
// bound, and returned from a profile.

fn grant_messages(src: &str) -> Vec<String> {
    raw_errors(src).iter().map(TypeError::message).collect()
}

fn grant_hints(src: &str) -> Vec<String> {
    raw_errors(src).iter().filter_map(TypeError::hint).collect()
}

const GRANT_SURFACE: &str = "[
    exec: [
        allow: [`name git, `absolute /bin/sh, `cwd tools/build, `dir `system, `dir `path],
        deny: [`name bash, `dir `absolute /opt/homebrew],
    ],
    fs: [
        read: [`cwd ., `xdg-data ., `home .gitconfig, `gitdir .],
        write: [`cwd build, `tempdir .],
        deny: [`cwd .git],
    ],
    net: `offline,
    gpu: `withheld,
    editor: [`read, `tui],
    shell: [`chdir],
]";

#[test]
fn grant_accepts_the_typed_surface_inline_computed_and_as_a_profile() {
    let inline = format!("grant {GRANT_SURFACE} {{ return 1 }}");
    assert_eq!(grant_messages(&inline), Vec::<String>::new());
    let computed = "let tool = 'rg'\nlet shared = [`absolute /usr/share]\n\
                    grant [exec: [allow: [`name $tool]], fs: [read: [...$shared]]] { return 1 }";
    assert_eq!(grant_messages(computed), Vec::<String>::new());
    let profile = contract_errors(declared(Form::Grant), &format!("return {GRANT_SURFACE}"));
    assert!(profile.is_empty(), "{profile:?}");
}

#[test]
fn grant_names_a_misspelt_place_in_the_places_words() {
    let sources = [
        "grant [fs: [read: [`cwdd .]]] { return 1 }",
        "let t = [read: [`cwdd .]]\ngrant [fs: $t] { return 1 }",
    ];
    for src in sources {
        let (msgs, hints) = (grant_messages(src), grant_hints(src));
        assert_eq!(msgs.len(), 1, "{src}: {msgs:?}");
        assert!(
            msgs[0].starts_with(
                "`` `cwdd `` is not a place; a place is one of `` `absolute ``, `` `cwd ``, "
            ),
            "{msgs:?}"
        );
        assert!(msgs[0].ends_with("(did you mean `` `cwd ``?)"), "{msgs:?}");
        assert!(
            hints.is_empty(),
            "the vocabulary replaces the row hint: {hints:?}"
        );
    }
    let errs = contract_errors(declared(Form::Grant), "return [fs: [read: [`cwdd .]]]");
    assert_eq!(codes_of(&errs), ["T0020"], "{errs:?}");
    assert!(errs[0].message().starts_with("`` `cwdd `` is not a place"));
}

/// A clash at a closed row is worded as a tag wherever ral meets one, not
/// only under a vocabulary.
#[test]
fn a_variant_clash_is_worded_as_a_tag() {
    let errs = raw_errors("grant [net: `online] { return 1 }");
    assert_eq!(codes_of(&errs), ["T0020"], "{errs:?}");
    assert_eq!(
        errs[0].kind.render_message(),
        "this value cannot be the tag `` `online ``"
    );
    assert!(
        errs[0]
            .message()
            .starts_with("`` `online `` is not a net mode")
    );
}

#[test]
fn grant_steers_a_wrongly_spelt_list_entry_to_its_tags() {
    let hints = grant_hints("grant [fs: [read: ['/tmp']]] { return 1 }");
    assert!(hints.iter().any(|h| h.contains("`absolute")), "{hints:?}");
    let hints = grant_hints("grant [exec: [allow: ['git']]] { return 1 }");
    assert!(
        hints
            .iter()
            .any(|h| h.contains("`name") && h.contains("`cwd")),
        "{hints:?}"
    );
    let hints = grant_hints("grant [net: true] { return 1 }");
    assert!(
        hints
            .iter()
            .any(|h| h.contains("omit `net` to inherit, or write `` net: `offline ``")),
        "{hints:?}"
    );
}

#[test]
fn grant_offers_dir_for_a_selector_in_an_exec_list_and_places_in_an_fs_list() {
    for selector in ["path", "system", "homebrew"] {
        let hints = grant_hints(&format!(
            "grant [exec: [allow: [`{selector}]]] {{ return 1 }}"
        ));
        let dir = format!("write `` `dir `{selector} ``");
        assert!(hints.iter().any(|h| h.contains(&dir)), "{hints:?}");
        let hints = grant_hints(&format!("grant [fs: [read: [`{selector}]]] {{ return 1 }}"));
        assert!(
            hints
                .iter()
                .any(|h| h.contains("fs lists take places such as `` `absolute `` or `` `cwd ``")),
            "{hints:?}"
        );
        assert!(
            hints
                .iter()
                .any(|h| h.contains(&format!("as `` `dir `{selector} ``"))),
            "{hints:?}"
        );
    }
}

/// A mismatch against a vocabulary is worded as the decoder words it.
#[test]
fn grant_words_a_type_mismatch_in_the_vocabularys_words() {
    let msgs = grant_messages("grant [net: true] { return 1 }");
    assert_eq!(msgs, ["expected a net mode, got a Bool"]);
    let msgs = grant_messages("grant [editor: true] { return 1 }");
    assert_eq!(msgs, ["expected a list of verbs, got a Bool"]);
    let msgs = grant_messages("grant [exec: [allow: ['git']]] { return 1 }");
    assert_eq!(msgs, ["expected an exec target, got text (String)"]);
}

#[test]
fn grant_holds_gpu_to_withheld() {
    assert_eq!(
        grant_messages("grant [gpu: true] { return 1 }"),
        ["expected a GPU mode, got a Bool"]
    );
    let hints = grant_hints("grant [gpu: true] { return 1 }");
    assert!(
        hints
            .iter()
            .any(|h| h.contains("omit `gpu` to inherit, or write `` gpu: `withheld ``")),
        "{hints:?}"
    );
    let errs = raw_errors("grant [gpu: `on] { return 1 }");
    assert!(
        errs[0].message().starts_with("`` `on `` is not a GPU mode"),
        "{errs:?}"
    );
}

/// `detach` is a shell verb: the checker gives the advice the decoder does,
/// inline and as a profile.
#[test]
fn grant_steers_a_detach_option_to_the_shell_verb() {
    let errs = raw_errors("grant [detach: true] { return 1 }");
    assert_eq!(codes_of(&errs), ["T0026"], "{errs:?}");
    assert_eq!(errs[0].message(), "`grant` does not take 'detach'");
    let hint = errs[0].hint().unwrap();
    assert!(hint.contains("`detach` is a shell verb"), "{hint}");
    assert!(hint.contains("`` shell: [`chdir, `detach] ``"), "{hint}");
    let errs = contract_errors(declared(Form::Grant), "return [detach: true]");
    assert_eq!(codes_of(&errs), ["T0026"], "{errs:?}");
}

/// Argument restrictions are gone: `only` is an unknown key of `exec`, with
/// no per-command advice and a suggestion only for a near miss.
#[test]
fn grant_refuses_an_exec_only_list_as_an_unknown_key() {
    let src = "grant [exec: [only: [`name git]]] { return 1 }";
    let errs = raw_errors(src);
    assert_eq!(codes_of(&errs), ["T0076"], "{errs:?}");
    assert!(errs[0].message().contains("unknown key 'only'"), "{errs:?}");
    assert!(errs[0].hint().is_none(), "{:?}", errs[0].hint());
    let errs = raw_errors("grant [exec: [alow: []]] { return 1 }");
    assert!(
        errs[0].message().ends_with("(did you mean 'allow'?)"),
        "{errs:?}"
    );
}

#[test]
fn grant_words_a_dir_payload_clash_in_the_dir_payloads_vocabulary() {
    for payload in ["`name git", "`dir `cwd tools"] {
        let src = format!("grant [exec: [allow: [`dir {payload}]]] {{ return 1 }}");
        let msgs = grant_messages(&src);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("is not a place or a selector"), "{msgs:?}");
    }
}

#[test]
fn grant_refuses_an_unknown_verb_with_the_nearest_one() {
    let msgs = grant_messages("grant [editor: [`wirte]] { return 1 }");
    assert!(
        msgs[0].starts_with("`` `wirte `` is not a verb"),
        "{msgs:?}"
    );
    assert!(
        msgs[0].ends_with("(did you mean `` `write ``?)"),
        "{msgs:?}"
    );
    let msgs = grant_messages("grant [shell: [`chdr]] { return 1 }");
    assert!(
        msgs[0].ends_with("(did you mean `` `chdir ``?)"),
        "{msgs:?}"
    );
}

/// A tag's payload is typed wherever the table sits: bound, or returned from
/// a profile.
#[test]
fn grant_refuses_a_wrongly_typed_payload_in_a_bound_table_and_a_profile() {
    for (axis, table) in [
        ("fs", "[read: [`cwd 1]]"),
        ("exec", "[allow: [`name false]]"),
    ] {
        let bound = raw_errors(&format!(
            "let t = {table}\ngrant [{axis}: $t] {{ return 1 }}"
        ));
        assert_eq!(codes_of(&bound), ["T0010"], "bound {table}: {bound:?}");
        let profile = contract_errors(declared(Form::Grant), &format!("return [{axis}: {table}]"));
        assert_eq!(
            codes_of(&profile),
            ["T0010"],
            "profile {table}: {profile:?}"
        );
    }
}

#[test]
fn grant_refuses_a_place_given_no_payload() {
    let errs = raw_errors("grant [fs: [read: [`cwd]]] { return 1 }");
    assert_eq!(codes_of(&errs), ["T0010"], "{errs:?}");
}

/// A nested table is a record, as a profile is: a map and `()` are refused
/// with the record spelling.
#[test]
fn grant_refuses_a_map_or_unit_as_a_nested_table() {
    for table in ["[:]", "[:, allow: []]", "()"] {
        let src = format!("grant [exec: {table}] {{ return 1 }}");
        let errs = raw_errors(&src);
        assert_eq!(codes_of(&errs), ["T0079"], "{table}: {errs:?}");
        let hint = errs[0].hint().unwrap();
        assert!(hint.contains("a grant table is a record"), "{hint}");
        assert!(hint.contains("exec: [allow: []]"), "{hint}");
    }
    let errs = contract_errors(declared(Form::Grant), "return [fs: ()]");
    assert_eq!(codes_of(&errs), ["T0079"], "{errs:?}");
    assert!(errs[0].hint().unwrap().contains("fs: [read: []]"));
}

#[test]
fn grant_shares_the_nested_tables_unknown_key_facts() {
    let errs = raw_errors("grant [exec: [allwo: []]] { return 1 }");
    assert_eq!(codes_of(&errs), ["T0076"], "{errs:?}");
    let Some(ral_core::typecheck::contract::Holds::Table(table)) =
        declared(Form::Grant).holds("exec")
    else {
        panic!("exec holds a nested table");
    };
    let TypeErrorKind::UnknownKey { form, key, offered } = &errs[0].kind else {
        panic!("expected unknown-key facts: {errs:?}");
    };
    let expected = table.unknown_key("allwo");
    assert_eq!(*form, table.form);
    assert_eq!(key, &expected.field);
    assert_eq!(
        offered
            .iter()
            .copied()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        expected.offered
    );
}

/// The nested check reads the table's type as it then stands, so a later
/// statement that fixes it is seen whichever side of the form it sits.
#[test]
fn grant_nested_check_does_not_depend_on_statement_order() {
    let form = "grant [exec: $t] { return 1 }";
    let fix = "let _ = [$t, [allow: [`cwdd .]]]";
    for body in [format!("{form}\n{fix}"), format!("{fix}\n{form}")] {
        let src = format!("let f = {{ |t|\n{body}\n}}");
        let errs = raw_errors(&src);
        assert_eq!(codes_of(&errs), ["T0020"], "{src}: {errs:?}");
    }
}

/// A table typed at a variable is never constrained and never refused: a
/// generic function's parameter is the runtime decoder's.
#[test]
fn grant_leaves_an_unresolved_table_to_the_decoder() {
    let src = "let exec-of = { |table| grant [exec: $table] { return 1 } }\n\
               let _ = exec-of [allow: [`name git]]\n\
               let _ = exec-of [allow: [`cwdd .]]";
    assert_eq!(grant_messages(src), Vec::<String>::new());
}

// ─── IR capture annotation ────────────────────────────────────────────────────
//
// Binding inference wraps each command a `let` captures in a `Capture` node,
// at any depth, and nothing else: a statement, a bound function, a forced
// block in hand are left as they are.

/// Visit every `Comp` reachable from an annotated toplevel's phrases —
/// [`common::walk_comp`] from each phrase's own root.
fn walk_toplevel(top: &Toplevel, visit: &mut impl FnMut(&Comp)) {
    for phrase in &top.phrases {
        match &phrase.item {
            Phrase::Define { comp, .. } | Phrase::Run(comp) => common::walk_comp(comp, visit),
        }
    }
}

/// How many `Capture` nodes the checker wrote, anywhere in the tree.
fn captures(src: &str) -> usize {
    let mut found = 0;
    walk_toplevel(&toplevel_ok(src), &mut |c| {
        if matches!(c.item, CompKind::Capture(_)) {
            found += 1;
        }
    });
    found
}

#[test]
fn a_let_captures_the_command_that_produces_its_value() {
    assert_eq!(captures("let x = echo hi"), 1);
    assert_eq!(captures("let f = { let x = echo hi; $x }"), 1);
    assert_eq!(captures("let f = { echo hi }; let x = f"), 1);
    assert_eq!(captures("let t = { echo hi }; let x = !$t"), 1);
    assert_eq!(captures("let g = { |n| echo $n }; let x = g 5"), 1);
}

#[test]
fn redirect_operands_are_inferred_without_a_string_requirement() {
    for head in ["echo hi", "!{ return () }"] {
        for redirect in ["<", "<<", ">", ">>", ">~", "2>", "2>>", "2>~"] {
            has_error(&format!("{head} {redirect} $missing"), "missing");
            for operand in ["42", "()", "[1, 2]", "{ return () }"] {
                toplevel_ok(&format!(
                    "let target = {operand}; {head} {redirect} $target"
                ));
            }
        }
        toplevel_ok(&format!("{head} 2>&1"));
    }
}

#[test]
fn case_joins_use_known_payloads_without_capturing_arms() {
    // A free Unit producer joins a command in `case` too.
    for (payload, body) in [("()", "return $p"), ("{ echo hi }", "!$p")] {
        for arms in [
            format!("`a: {{ |p| {body} }}, `b: {{ |_| echo bye }}"),
            format!("`b: {{ |_| echo bye }}, `a: {{ |p| {body} }}"),
        ] {
            let src = format!("let f = {{ case `a {payload} [{arms}] }}");
            assert_eq!(scheme_at(&toplevel_ok(&src), 0), "Command");
            assert_eq!(captures(&src), 0);
        }
    }

    // `case` cannot capture its command arm (SPEC §17.6).
    // Source order blames the later arm, even with a known payload.
    for (arms, hint, later) in [
        (
            "`a: { |p| return $p }, `b: { |_| echo bye }",
            "this arm is a command and an earlier one returns a value; an arm's output is not captured: return it, `{ return !{…} }`",
            "echo bye",
        ),
        (
            "`b: { |_| echo bye }, `a: { |p| return $p }",
            "this arm returns a value and an earlier one is a command; an arm's output is not captured: make that one return it, `{ return !{…} }`",
            "return $p",
        ),
    ] {
        let src = format!("let f = {{ case `a hello [{arms}] }}");
        let err = one_error_with_hint(&src, "T0011", hint);
        assert_error_in(&src, &err, later);

        let repaired = src.replace("echo bye", "return !{echo bye}");
        assert_eq!(scheme_at(&toplevel_ok(&repaired), 0), "Returns String");
        assert_eq!(captures(&repaired), 1);
    }
}

#[test]
fn redirects_scope_ready_computations_not_partial_applications() {
    for redirect in ["> out", "2> out", "< input", "<< 'text'", "2>&1"] {
        let src = format!("let f = {{ |a b| echo \"$a $b\" }}; let g = f left {redirect}");
        let errors = raw_errors(&src);
        assert!(
            errors
                .iter()
                .any(|e| e.hint().is_some_and(|h| h.contains("redirect needs"))),
            "{src}: {errors:?}"
        );
        toplevel_ok(&format!(
            "let f = {{ |a b| echo \"$a $b\" }}; let g = {{ |b| f left $b {redirect} }}"
        ));
    }
}

#[test]
fn abstract_stdout_discharge_has_a_fresh_result_grade() {
    let top =
        toplevel_ok("let f = { |t| !$t > out }; let x = f { echo hi }; let n = f { return 42 }");
    // Redirecting preserves the value type (SPEC §17.3),
    // but its result grade is independent of the input's.
    assert_eq!(scheme_at(&top, 0), "∀α ν ξ. {Command[ν] α} → Command[ξ] α");
    assert_eq!(scheme_at(&top, 1), "Unit");
    assert_eq!(scheme_at(&top, 2), "Integer");
    assert_eq!(
        captures("let f = { |t| !$t > out }; let x = f { echo hi }"),
        0
    );
    has_error(
        "let f = { |t| !$t > out }; let x = f { echo hi }; echo $x",
        "cannot pass Unit as an argument to `echo`",
    );
}

#[test]
fn a_statement_and_a_redirected_command_capture_nothing() {
    assert_eq!(captures("echo hi"), 0);
    assert_eq!(captures("let f = { echo hi; echo there }"), 0);
    assert_eq!(captures("let x = to-json 1 > f"), 0);
}

/// One `Capture` around the whole right-hand side, however it branches.
#[test]
fn a_let_captures_a_pipeline_and_a_join_whole() {
    assert_eq!(captures("let x = echo hi | cat"), 1);
    assert_eq!(captures("let x = if true { echo a } else { echo b }"), 1);
    assert_eq!(captures("let x = try { echo a } { |e| echo b }"), 1);
    assert_eq!(
        captures("let t = { echo a }; let u = { echo b }; let x = if true $t else $u"),
        1
    );
}

/// Literal and in-hand `map` callbacks are never implicitly captured.
#[test]
fn map_command_callbacks_are_not_implicitly_captured() {
    for (src, index) in [
        ("let xs = map { |f| echo $f } [1, 2]", 0),
        ("let g = { |n| echo $n }; let xs = map $g [1, 2]", 1),
    ] {
        assert_eq!(scheme_at(&toplevel_ok(src), index), "[Unit]");
        assert_eq!(captures(src), 0);
    }
}

#[test]
fn map_callbacks_capture_only_with_an_explicit_let() {
    // Return a hoisted capture to request strings per item from `map`.
    for (src, index) in [
        ("let xs = map { |f| return !{echo $f} } [1, 2]", 0),
        (
            "let g = { |n| echo $n }; let xs = map { |n| return !{g $n} } [1, 2]",
            1,
        ),
    ] {
        assert_eq!(scheme_at(&toplevel_ok(src), index), "[String]");
        assert_eq!(captures(src), 1);
    }
}

#[test]
fn in_hand_command_arms_are_not_implicitly_captured() {
    // A command/String `if` join with the command block already in hand.
    let src = "let t = { echo a }; let x = if true $t else { return b }";
    let err = one_error_with_hint(
        src,
        "T0011",
        "this branch returns a value and an earlier one is a command; a branch's output is not captured: make that one return it, `{ return !{…} }`",
    );
    assert_error_in(src, &err, "return b");
    let repaired = "let t = { echo a }; let x = if true { return !{!$t} } else { return b }";
    assert_eq!(scheme_at(&toplevel_ok(repaired), 1), "String");
    assert_eq!(captures(repaired), 1);
}

#[test]
fn value_join_syntaxes_preserve_shape_hints_and_blame() {
    let (left, right) = ("return [:, a: 'x']", "return [a: 'x']");
    for (first, later) in [(left, right), (right, left)] {
        for src in [
            format!("let x = if true {{ {first} }} else {{ {later} }}"),
            format!("let x = case `a () [`a: {{ |_| {first} }}, `b: {{ |_| {later} }}]"),
            format!("let x = try {{ {first} }} {{ |_| {later} }}"),
            format!("let x = {first} ? {later}"),
        ] {
            let errs = raw_errors(&src);
            assert_eq!(codes_of(&errs), ["T0011"], "{src}: {errs:?}");
            assert!(
                matches!(&errs[0].kind, TypeErrorKind::CompTyMismatch { expected, actual }
                    if matches!((expected.node(), actual.node()),
                        (CompTyNode::Return(g, _), CompTyNode::Return(h, _)) if g == h)),
                "{src}: {errs:?}"
            );
            assert!(
                errs[0]
                    .hint()
                    .is_some_and(|h| h.contains("a record and a map are different types")),
                "{src}: {errs:?}"
            );
            assert_error_in(&src, &errs[0], later);
        }
    }
}

#[test]
fn block_result_mismatches_keep_specific_argument_hints() {
    for (src, fragment) in [
        (
            "let f = { |b| let xs = !{!$b}; return [...$xs] }; f { return text }",
            "`lines $s`",
        ),
        (
            "let f = { |b| let r = !{!$b}; string-replace 'x' 'y' $r[name] }; f { return text }",
            "`$x[name]`",
        ),
    ] {
        let errs = raw_errors(src);
        assert_eq!(codes_of(&errs), ["T0011"], "{src}: {errs:?}");
        assert!(
            errs[0].hint().is_some_and(|h| h.contains(fragment)),
            "{src}: {errs:?}"
        );
    }
}

#[test]
fn try_command_and_string_outcomes_require_explicit_capture() {
    // A mixed `try` cannot capture its command outcome (SPEC §17.6).
    // The later outcome is blamed in either order.
    for (src, hint, later) in [
        (
            "let x = try { hostname } { |e| return none }",
            "this outcome returns a value and an earlier one is a command; an outcome's output is not captured: make that one return it, `{ return !{…} }`",
            "return none",
        ),
        (
            "let x = try { return none } { |e| hostname }",
            "this outcome is a command and an earlier one returns a value; an outcome's output is not captured: return it, `{ return !{…} }`",
            "hostname",
        ),
    ] {
        let err = one_error_with_hint(src, "T0011", hint);
        assert_error_in(src, &err, later);
        let repaired = src.replace("hostname", "return !{hostname}");
        assert_eq!(scheme_at(&toplevel_ok(&repaired), 0), "String");
        assert_eq!(captures(&repaired), 1);
    }
}

// Capture and grade typing pins (SPEC §§17.3–17.6).

const COMMAND_AFTER_VALUE_BRANCH_HINT: &str = "this branch is a command and an earlier one returns a value; a branch's output is not captured: return it, `{ return !{…} }`";
const VALUE_AFTER_COMMAND_BRANCH_HINT: &str = "this branch returns a value and an earlier one is a command; a branch's output is not captured: make that one return it, `{ return !{…} }`";
const COMMAND_AFTER_INT_BRANCH_HINT: &str = "this branch is a command and an earlier one returns an Int; give this branch a value of that type too (`… | from-json`, or `return` one), or write the earlier one's value out with `echo`";
const INT_AFTER_COMMAND_BRANCH_HINT: &str = "this branch returns an Int and an earlier one is a command; give that one a value of that type too (`… | from-json`, or `return` one), or write this branch's value out with `echo`";
const COMMAND_VALUE_ARGUMENT_HINT: &str = "this block is a command, but a block that returns a value is wanted here: return its output, `{ |…| return !{…} }`";

#[test]
fn command_compatible_unit_producers_join_without_capture() {
    // Unit producers join commands in `try` and after binding.
    for src in [
        "let f = { try { each-line { |l| warn $l } } { |e| echo failed } }",
        "let f = { let u = (); let c = true; if $c { return $u } else { echo a } }",
        "let f = { let u = (); let c = true; if $c { echo a } else { return $u } }",
    ] {
        assert_eq!(scheme_at(&toplevel_ok(src), 0), "Command");
        assert_eq!(captures(src), 0);
    }
    // A heterogeneous block list permits a free unit grade to become a command.
    assert_eq!(captures("each { |b| !$b } [{ echo hi }, { return () }]"), 0);
}

#[test]
fn command_and_string_joins_require_explicit_capture_in_both_orders() {
    // A hostname/String `if` join blames the later branch.
    for (first, later, hint) in [
        ("hostname", "return 'b'", VALUE_AFTER_COMMAND_BRANCH_HINT),
        ("return 'b'", "hostname", COMMAND_AFTER_VALUE_BRANCH_HINT),
    ] {
        let src = format!("let c = true; let x = if $c {{ {first} }} else {{ {later} }}");
        let err = one_error_with_hint(&src, "T0011", hint);
        assert_error_in(&src, &err, later);

        let repaired = src.replace("hostname", "return !{hostname}");
        assert_eq!(scheme_at(&toplevel_ok(&repaired), 1), "String");
        assert_eq!(captures(&repaired), 1);
    }
}

#[test]
fn command_and_int_joins_keep_directional_hints_in_both_orders() {
    // A refused command/Int join receives Int-specific advice.
    // Later branches are actual, not reordered to prefer commands.
    for (first, later, hint) in [
        ("echo a", "return 1", INT_AFTER_COMMAND_BRANCH_HINT),
        ("return 1", "echo a", COMMAND_AFTER_INT_BRANCH_HINT),
    ] {
        let src = format!("let c = true; let x = if $c {{ {first} }} else {{ {later} }}");
        let err = one_error_with_hint(&src, "T0011", hint);
        assert_error_in(&src, &err, later);
        let repaired = src.replace("echo a", "return !{echo a | from-json}");
        assert_eq!(scheme_at(&toplevel_ok(&repaired), 1), "Integer");
        assert_eq!(
            captures(&repaired),
            0,
            "the decoder returns data, not output"
        );
    }
}

#[test]
fn subtraction_by_one_joins_keep_int_hints_in_both_orders() {
    // Subtraction by a literal fixes the payload before a command/value join.
    // Here `1` fixes n to Int; the next test retains a genuine NUMBER variable.
    for (first, later, hint) in [
        (
            "return $[$n - 1]",
            "echo done",
            COMMAND_AFTER_INT_BRANCH_HINT,
        ),
        (
            "echo done",
            "return $[$n - 1]",
            INT_AFTER_COMMAND_BRANCH_HINT,
        ),
    ] {
        for bind_result in [false, true] {
            let join = format!("if $c {{ {first} }} else {{ {later} }}");
            let body = if bind_result {
                format!("let result = {join}; return $result")
            } else {
                join
            };
            let src = format!("let c = true; let f = {{ |n| {body} }}");
            let err = one_error_with_hint(&src, "T0011", hint);
            assert_error_in(&src, &err, later);
            toplevel_ok(&src.replace("echo done", "return 0"));
        }
    }
}

#[test]
fn number_kind_joins_keep_the_value_grade_in_both_orders() {
    // Kind admission protects command/value joins: n-n leaves NUMBER
    // free, unlike literal n-1; that variable still requires a value.
    for (first, later, hint) in [
        (
            "return $[$n - $n]",
            "echo done",
            COMMAND_AFTER_VALUE_BRANCH_HINT,
        ),
        (
            "echo done",
            "return $[$n - $n]",
            VALUE_AFTER_COMMAND_BRANCH_HINT,
        ),
    ] {
        for bind_result in [false, true] {
            let join = format!("if $c {{ {first} }} else {{ {later} }}");
            let body = if bind_result {
                format!("let result = {join}; return $result")
            } else {
                join
            };
            let src = format!("let c = true; let f = {{ |n| {body} }}");
            let err = one_error_with_hint(&src, "T0011", hint);
            assert_error_in(&src, &err, later);
            let TypeErrorKind::CompTyMismatch { expected, actual } = &err.kind else {
                panic!("expected a producer mismatch: {err:?}");
            };
            let (command, value) =
                if matches!(expected.node(), CompTyNode::Return(Grade::Output, _)) {
                    (expected, actual)
                } else {
                    (actual, expected)
                };
            assert!(
                matches!(command.node(), CompTyNode::Return(Grade::Output, ty) if *ty == Ty::unit())
            );
            assert!(
                matches!(value.node(), CompTyNode::Return(grade, ty) if *grade != Grade::Output && matches!(ty.node(), TyNode::Var(_)))
            );
            toplevel_ok(&src.replace("echo done", "return 0"));
        }
    }
}

#[test]
fn rejected_producer_equations_do_not_poison_later_command_joins() {
    for rejected_join in ["if true $g else $f", "if true $f else $g"] {
        let src = format!(
            "let broken = {{ |f g| \
             if true $f else {{ return 1 }}; \
             if true $g else {{ return () }}; \
             {rejected_join}; \
             if true $g else {{ echo hi }}; \
             let x = !$f; return $x }}"
        );
        let errs = raw_errors(&src);
        assert_eq!(codes_of(&errs), ["T0011"], "{src}: {errs:?}");
        let TypeErrorKind::CompTyMismatch { expected, actual } = &errs[0].kind else {
            panic!("expected a producer mismatch: {errs:?}");
        };
        let (CompTyNode::Return(a, ta), CompTyNode::Return(b, tb)) =
            (expected.node(), actual.node())
        else {
            panic!("expected two producers: {errs:?}");
        };
        assert!(
            matches!(
                (a, ta.node(), b, tb.node()),
                (Grade::Value, TyNode::Int, Grade::Var(_), TyNode::Unit)
                    | (Grade::Var(_), TyNode::Unit, Grade::Value, TyNode::Int)
            ),
            "only the Int producer's own payload entails Value: {errs:?}"
        );
    }
}

#[test]
fn record_command_default_conflict_is_reported_once_before_a_later_let() {
    let src = "let broken = { |f g| \
               to-json !{map $f []}; \
               if true { g () } else { return [fn: { return () }] }; \
               if true { f () } else { g () }; \
               if true { f () } else { echo hi }; \
               let x = g (); return $x }";
    let errs = raw_errors(src);
    assert_eq!(codes_of(&errs), ["T0011"], "{errs:?}");
    let span = errs[0].pos.expect("the conflicting application has a span");
    assert_eq!((span.start, span.end), (123, 125));
    assert_eq!(&src[span.start as usize..span.end as usize], "()");
    let TypeErrorKind::CompTyMismatch { expected, actual } = &errs[0].kind else {
        panic!("expected a record-versus-command mismatch: {errs:?}");
    };
    assert!(
        matches!((expected.node(), actual.node()),
            (CompTyNode::Return(Grade::Value, record), CompTyNode::Return(Grade::Output, unit))
            | (CompTyNode::Return(Grade::Output, unit), CompTyNode::Return(Grade::Value, record))
            if matches!(record.node(), TyNode::Record(_)) && *unit == Ty::unit()),
        "the default conflict, not the later let, must be reported: {errs:?}"
    );
}

#[test]
fn rejected_handle_equations_do_not_poison_a_later_await() {
    for rejected_join in [
        "if true { return $g } else { return $f }",
        "if true { return $f } else { return $g }",
    ] {
        let src = format!(
            "let broken = {{ |f g| \
             if true {{ return $f }} else {{ spawn {{ return 1 }} }}; \
             if true {{ return $g }} else {{ spawn {{ return () }} }}; \
             {rejected_join}; \
             if true {{ return $g }} else {{ spawn {{ echo hi }} }}; \
             let x = await $f; return $x }}"
        );
        let errs = raw_errors(&src);
        assert_eq!(codes_of(&errs), ["T0011"], "{src}: {errs:?}");
        let TypeErrorKind::CompTyMismatch { expected, actual } = &errs[0].kind else {
            panic!("expected a producer mismatch: {errs:?}");
        };
        let (CompTyNode::Return(_, a), CompTyNode::Return(_, b)) = (expected.node(), actual.node())
        else {
            panic!("expected two handle producers: {errs:?}");
        };
        let (TyNode::Handle(a, ta), TyNode::Handle(b, tb)) = (a.node(), b.node()) else {
            panic!("expected two handles: {errs:?}");
        };
        assert!(
            matches!(
                (a, ta.node(), b, tb.node()),
                (Grade::Value, TyNode::Int, Grade::Var(_), TyNode::Unit)
                    | (Grade::Var(_), TyNode::Unit, Grade::Value, TyNode::Int)
            ),
            "only the Int handle's own payload entails Value: {errs:?}"
        );
    }
}

#[test]
fn numeric_spawn_handle_lists_keep_grade_type_pairing_in_both_orders() {
    // Handle payloads and grades must remain paired (SPEC §17.3).
    // Kind admission protects Handle too; list mismatch advice still applies.
    let hint = "a list holds one type, and this element's differs from its neighbours': to keep different things side by side, write a record, as in `[name: $n, status: $s]`; for alternatives, tag each element and `case` on it downstream";
    for (expression, variable) in [("$[$n - 1]", false), ("$[$n - $n]", true)] {
        for handles in ["[$value, $command]", "[$command, $value]"] {
            let src = format!(
                "let f = {{ |n| let value = spawn {{ return {expression} }}; \
                 let command = spawn {{ echo done }}; return {handles} }}"
            );
            let err = one_error_with_hint(&src, "T0010", hint);
            let TypeErrorKind::TyMismatch { expected, actual } = &err.kind else {
                panic!("expected a handle mismatch: {err:?}");
            };
            let (command, value) = if matches!(expected.node(), TyNode::Handle(Grade::Output, _)) {
                (expected.node(), actual.node())
            } else {
                (actual.node(), expected.node())
            };
            assert!(matches!(command, TyNode::Handle(Grade::Output, ty) if *ty == Ty::unit()));
            let TyNode::Handle(grade, ty) = value else {
                panic!("expected a value-producing handle: {err:?}");
            };
            assert_ne!(*grade, Grade::Output);
            if variable {
                assert!(matches!(ty.node(), TyNode::Var(_)), "{err:?}");
            } else {
                assert_eq!(*ty, Ty::int());
            }
            toplevel_ok(&src.replace("echo done", "return 0"));
        }
    }
}

#[test]
fn twice_fixes_its_callbacks_grade_until_an_explicit_unit_return() {
    // `twice`'s inner let fixes the value grade, even for unit (SPEC §17.5).
    let src = "let twice = { |f| let _ = !$f; !$f }; let c = true; \
               if $c { twice { return () } } else { echo a }";
    let err = one_error_with_hint(
        src,
        "T0011",
        "this branch is a command and an earlier one returns nothing (`()`); give this branch a value of that type too (`… | from-json`, or `return` one), or write the earlier one's value out with `echo`",
    );
    assert_error_in(src, &err, "echo a");
    let repaired = src.replace("twice { return () }", "twice { return () }; return ()");
    toplevel_ok(&repaired);
    assert_eq!(captures(&repaired), 0);
}

#[test]
fn both_demands_value_blocks_without_implicit_capture() {
    // `both` refuses each command argument without implicit capture.
    for (args, errors) in [
        ("{ echo a } { return b }", 1),
        ("{ return a } { echo b }", 1),
        ("{ echo a } { echo b }", 2),
    ] {
        let src = format!("let pair = both {args}");
        let errs = raw_errors(&src);
        assert_eq!(codes_of(&errs), vec!["T0011"; errors], "{src}: {errs:?}");
        for err in errs {
            assert_eq!(err.hint().as_deref(), Some(COMMAND_VALUE_ARGUMENT_HINT));
        }
        let repaired = src
            .replace("{ echo a }", "{ return !{echo a} }")
            .replace("{ echo b }", "{ return !{echo b} }");
        assert_eq!(
            scheme_at(&toplevel_ok(&repaired), 0),
            "[fst: String, snd: String]"
        );
        assert_eq!(captures(&repaired), errors);
    }
}

#[test]
fn filter_demands_a_bool_block_without_implicit_capture() {
    // `filter` requires Bool-producing blocks (SPEC §17.6).
    let hint = "this block is a command, but a block that returns a Bool is wanted here; for a yes/no answer, `succeeds { … }`";
    for src in [
        "filter { |x| echo $x } [1, 2]",
        "let pred = { |x| echo $x }; filter $pred [1, 2]",
    ] {
        one_error_with_hint(src, "T0011", hint);
    }
    let repaired = "let xs = filter { |x| succeeds { echo $x } } [1, 2]";
    assert_eq!(scheme_at(&toplevel_ok(repaired), 0), "[Integer]");
    assert_eq!(captures(repaired), 0);
}

#[test]
fn decoder_results_cannot_join_commands_in_either_order() {
    // A boundary result has value grade, never command grade (SPEC §17.3).
    // Blame the later arm and retain the weak-boundary note.
    let weak_note = "this type is shared by every use in its unit, and another use fixed it: what `from-json` reads has one type for the whole unit, which its door checks every use of. Write one function per shape, or decode once and bind the result";
    for (first, later, hint) in [
        ("echo a", "from-json < f", VALUE_AFTER_COMMAND_BRANCH_HINT),
        ("from-json < f", "echo a", COMMAND_AFTER_VALUE_BRANCH_HINT),
    ] {
        let src = format!("let c = true; let x = if $c {{ {first} }} else {{ {later} }}");
        let err = one_error_with_hint(&src, "T0011", &format!("{hint}\n{weak_note}"));
        assert_error_in(&src, &err, later);
        let repaired = src.replace("echo a", "return !{echo a}");
        assert_eq!(scheme_at(&toplevel_ok(&repaired), 1), "String");
        assert_eq!(captures(&repaired), 1);
    }
}

#[test]
fn a_function_given_a_thunk_of_itself_keeps_the_argument_cycle_path() {
    for (src, relation) in [
        (
            include_str!("../../tests/reject/a-function-given-a-thunk-of-itself.ral"),
            "takes itself as an argument",
        ),
        (
            include_str!("../../tests/reject/a-function-applied-to-itself.ral"),
            "is applied to itself",
        ),
        (
            include_str!("../../tests/reject/a-function-that-returns-itself.ral"),
            "returns itself",
        ),
    ] {
        let errs = raw_errors(src);
        assert_eq!(codes_of(&errs), ["T0073"], "{src}: {errs:?}");
        assert!(errs[0].message().contains(relation), "{src}: {errs:?}");
    }
}

#[test]
fn type_nesting_too_deep_by_doubling_does_not_accuse_the_head_of_being_data() {
    let src = include_str!("../../tests/reject/type-nesting-too-deep-by-doubling.ral");
    let errs = raw_errors(src);
    assert_eq!(codes_of(&errs), ["T0003"], "{errs:?}");
    assert_error_in(src, &errs[0], "d10");
    let hint = errs[0].hint().expect("depth guidance");
    assert!(hint.contains("reduce the type's nesting"), "{hint}");
    assert!(!hint.contains("head must be a block"), "{hint}");
}

#[test]
fn recursive_calls_keep_monomorphic_advice_at_the_narrower_argument_spans() {
    for (src, code, argument) in [
        (
            include_str!("../../tests/reject/inner-bind-keeps-the-enclosing-groups-join.ral"),
            "T0011",
            "0",
        ),
        (
            include_str!("../../tests/reject/sibling-arms-grounding-apart.ral"),
            "T0011",
            "0",
        ),
        (
            include_str!("../../tests/reject/late-byte-arm-beside-value-payload-arm-under-try.ral"),
            "T0011",
            "$v",
        ),
        (
            include_str!("../../tests/reject/recursive-definition-single-self-type.ral"),
            "T0010",
            "\"s\"",
        ),
        (
            include_str!(
                "../../tests/reject/recursive-function-is-monomorphic-within-its-body.ral"
            ),
            "T0010",
            "\"s\"",
        ),
        (
            "let rec = { |n| if $[$n == 0] { return 0 } else { $rec \"s\" } }",
            "T0010",
            "\"s\"",
        ),
    ] {
        let errs = raw_errors(src);
        assert_eq!(codes_of(&errs), [code], "{src}: {errs:?}");
        assert_error_in(src, &errs[0], argument);
        assert!(
            errs[0].hint().is_some_and(|hint| hint.contains(
                "a definition that calls itself is checked at one single type throughout its own body"
            )),
            "{src}: {errs:?}"
        );
    }
}

#[test]
fn recursive_body_context_does_not_relabel_nonrecursive_or_shadowed_calls() {
    for src in [
        "let int = { |x| return $[$x + 1] }; let f = { |n| let _ = !{f $n}; int \"s\" }",
        "let f = { |n| let _ = !{f $n}; let f = { |x| return $[$x + 1] }; f \"s\" }",
        "let f = { |n| if $[$n == 0] { return 0 } else { f $[$n - 1] } }; f \"s\"",
        "let f = { |n| let _ = !{f $n}; return $[1 + \"s\"] }",
    ] {
        let errs = raw_errors(src);
        assert_eq!(codes_of(&errs), ["T0010"], "{src}: {errs:?}");
        assert!(
            errs.iter().all(|err| !err
                .hint()
                .is_some_and(|hint| hint.contains("a definition that calls itself"))),
            "{src}: {errs:?}"
        );
    }
}
