//! Behavioural oracle for the HM static type checker: what the checker
//! computes and annotates, read off the checker's own data.
//!
//! Every test parses + elaborates a small ral program and runs `typecheck()`.
//! What the checker *refuses*, and the words it refuses in, lives in the
//! corpus of programs under `tests/reject/` and `tests/accept/`, run by
//! `ral/tests/corpus.rs`.

#![allow(clippy::disallowed_methods, reason = "test code")]

mod common;

use ral_core::ir::{Comp, CompKind, Phrase, Toplevel, Unchecked, Val};
use ral_core::ty::Ty;
use ral_core::typecheck::Form;
use ral_core::typecheck::contract::declared;
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
    let comp =
        elaborate(&ast, [], None).unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
    typecheck(&comp, common::schemes_for(surface), None)
        .err()
        .unwrap_or_default()
}

/// A surface holding `detach`, the base frame core publishes but does not
/// install: a host takes it together with the birth budget it spends.
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

/// `par` demands a value of its callback, so a command's output is what each
/// item maps to.
#[test]
fn par_demands_a_value_and_returns_a_list() {
    let top = toplevel_ok("let xs = par { |x| echo $x } [1, 2] 2\nreturn ()");
    assert_eq!(scheme_at(&top, 0), "[String]");
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
    assert_eq!(scheme_at(&top, 0), "∀α. Map α → String → Returns α");
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
    assert_eq!(scheme_at(&top, 0), "∀α. [α] → Returns α");
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
        assert_eq!(scheme_at(&top, 0), "∀α. [α] → Integer → Returns α");
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
            holds: ral_core::typecheck::contract::Holds::At(Ty::Int),
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
            holds: ral_core::typecheck::contract::Holds::Required(Ty::String),
            reason: None,
        },
        ral_core::typecheck::contract::Key {
            label: "loose",
            holds: ral_core::typecheck::contract::Holds::Decoded,
            reason: None,
        },
        ral_core::typecheck::contract::Key {
            label: "banned",
            holds: ral_core::typecheck::contract::Holds::Refused("write it somewhere else"),
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

/// A key the table knows and refuses says so in its own words: folding it into
/// "unknown key, here is the list" would lose the only advice it carries.
#[test]
fn return_schema_keeps_a_refused_keys_own_sentence() {
    let errs = contract_errors(&REQUIRED_TABLE, "return [must: 'here', banned: 1]");
    let refusal = errs
        .iter()
        .find(|e| e.kind.code() == "T0026")
        .unwrap_or_else(|| panic!("expected T0026, got: {errs:?}"));
    assert_eq!(
        refusal.hint().as_deref(),
        Some("write it somewhere else"),
        "the table's own advice is the message"
    );
    assert_eq!(errs.len(), 1, "and it is not also an unknown key: {errs:?}");
}

/// A key the table does not name is said in the table's own wording, the one
/// the runtime checks use.
#[test]
fn return_schema_names_an_unknown_key_in_the_tables_words() {
    let errs = schema_errors("return [nn: 1]");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].kind.code(), "T0076");
    assert_eq!(errs[0].kind.render_message(), TEST_TABLE.unknown_key("nn"),);
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
fn grant_names_an_unknown_nested_key_in_that_tables_words() {
    let errs = raw_errors("grant [exec: [allwo: []]] { return 1 }");
    assert_eq!(codes_of(&errs), ["T0076"], "{errs:?}");
    assert_eq!(
        errs[0].kind.render_message(),
        declared(Form::Grant)
            .holds("exec")
            .and_then(|h| match h {
                ral_core::typecheck::contract::Holds::Table(t) => Some(t.unknown_key("allwo")),
                _ => None,
            })
            .unwrap()
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
// The annotation pass wraps each command a `let` captures in a `Capture` node,
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
fn unit_producers_have_explicit_upcasts_that_survive_reinference() {
    for src in [
        "let x = if true { return () } else { echo hi }",
        "case `a () [`a: { |_| return () }, `b: { |_| echo hi }]",
        "within [handlers: [foo: { |args| return () }]] { foo }",
        "let f = { |args| return () }; within [handlers: [foo: $f]] { foo }",
        "let f = { |args| return () }; within [handlers: [foo: { !$f }]] { foo }",
    ] {
        let top = toplevel_ok(src);
        let count = |top: &Toplevel| {
            let mut found = 0;
            walk_toplevel(top, &mut |comp| {
                if matches!(comp.item, CompKind::Upcast(_)) {
                    found += 1;
                }
            });
            found
        };
        assert_eq!(count(&top), 1, "{src}");
        let unchecked: Unchecked = top
            .phrases
            .iter()
            .map(|phrase| ral_core::source::Spanned {
                span: phrase.span,
                item: match phrase.item.clone() {
                    Phrase::Define { pattern, comp, .. } => Phrase::Define {
                        pattern,
                        comp,
                        schemes: (),
                    },
                    Phrase::Run(comp) => Phrase::Run(comp),
                },
            })
            .collect();
        let checked = typecheck(
            &unchecked,
            common::schemes_for(&ral_core::HostSurface::default()),
            None,
        )
        .unwrap_or_else(|errors| panic!("re-inference of {src}: {errors:?}"));
        assert_eq!(count(&checked), 1, "{src}");
    }
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
fn case_joins_use_known_payloads_before_deciding_capture() {
    for (payload, body, expected) in [
        ("()", "return $p", "Command"),
        ("hello", "return $p", "Returns String"),
        ("{ echo hi }", "!$p", "Command"),
    ] {
        for arms in [
            format!("`a: {{ |p| {body} }}, `b: {{ |_| echo bye }}"),
            format!("`b: {{ |_| echo bye }}, `a: {{ |p| {body} }}"),
        ] {
            let top = toplevel_ok(&format!("let f = {{ case `a {payload} [{arms}] }}"));
            assert_eq!(scheme_at(&top, 0), expected);
        }
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
fn abstract_stdout_discharge_preserves_the_input_grade() {
    let top =
        toplevel_ok("let f = { |t| !$t > out }; let x = f { echo hi }; let n = f { return 42 }");
    assert_eq!(scheme_at(&top, 1), "Unit");
    assert_eq!(scheme_at(&top, 2), "Integer");
    assert_eq!(
        captures("let f = { |t| !$t > out }; let x = f { echo hi }"),
        0
    );
    has_error(
        "let f = { |t| !$t > out }; let x = f { echo hi }; echo $x",
        "nothing",
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

/// A value demanded of a command block captures the block's innermost body,
/// or η-wraps a block in hand around a captured call.
#[test]
fn a_value_demand_captures_a_block_argument_or_arm() {
    assert_eq!(captures("map { |f| echo $f } [1, 2]"), 1);
    assert_eq!(captures("let g = { |n| echo $n }; map $g [1, 2]"), 1);
    assert_eq!(
        captures("let t = { echo a }; let x = if true $t else { return b }"),
        1
    );
    assert_eq!(captures("let x = try { hostname } { |e| return none }"), 1);
}
