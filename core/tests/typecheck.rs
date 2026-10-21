//! Behavioural oracle for the HM static type checker: what the checker
//! computes and annotates, read off the checker's own data.
//!
//! Every test parses + elaborates a small ral program and runs `typecheck()`.
//! What the checker *refuses*, and the words it refuses in, lives in the
//! corpus of programs under `tests/reject/` and `tests/accept/`, run by
//! `ral/tests/corpus.rs`.

mod common;

use ral_core::ir::{Comp, CompKind, Pattern, Phrase, Toplevel, Unchecked, Val};
use ral_core::ty::{CompTy, CompTyVar, Ty};
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
        statics: vec![ral_core::builtins::DETACH_BUILTIN],
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

#[test]
fn fmt_scheme_shows_quantified_comp_vars() {
    let beta = CompTyVar(17);
    let scheme = ral_core::test_access::scheme_over_comp_vars(
        vec![beta],
        Ty::Thunk(Box::new(CompTy::Var(beta))),
        vec![],
    );
    let rendered = scheme.to_string();
    assert_eq!(rendered, "∀ϕ. ϕ");
}

#[test]
fn fmt_scheme_binds_cyclic_comp_roots_by_mu() {
    let root = CompTyVar(29);
    let binding = CompTy::pure(Ty::list(Ty::Thunk(Box::new(CompTy::Var(root)))));
    let scheme = ral_core::test_access::scheme_over_comp_vars(
        vec![],
        Ty::Thunk(Box::new(binding.clone())),
        vec![(root, binding)],
    );
    let rendered = scheme.to_string();
    assert_eq!(rendered, "μϕ. Returns [{ϕ}]");
}

/// `workers` is data: a row's fields read at their documented types, including
/// the three-state `state` variant (a consumed worker is never listed), and neither `worker` nor `worker-command`
/// exists in the builtin manifest.
#[test]
fn workers_lists_rows_at_their_documented_types_and_no_recovery_builtin_exists() {
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
    let manifest = common::manifest(&ral_core::HostSurface::default());
    assert!(manifest.get("worker").is_none());
    assert!(manifest.get("worker-command").is_none());
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
    assert!(
        !raw_errors(old_two_arm).is_empty(),
        "old two-arm `pending`/`settled` poll case must be rejected"
    );

    let missing_stopping = "let h = spawn { return 1 }\n\
                            case !{poll $h} [\n\
                                `running: { |_| return 0 },\n\
                                `ready: { |_| return 1 },\n\
                                `consumed: { |_| return 2 }\n\
                            ]";
    assert!(
        !raw_errors(missing_stopping).is_empty(),
        "omitting `stopping` from `poll` case must fail exhaustiveness"
    );
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
    let (
        Phrase::Define {
            schemes: f_schemes, ..
        },
        Phrase::Define {
            schemes: g_schemes, ..
        },
    ) = (&top.phrases[0].item, &top.phrases[1].item)
    else {
        panic!("expected two Define phrases");
    };
    let f_rendered = f_schemes[0].1.to_string();
    let g_rendered = g_schemes[0].1.to_string();
    assert!(
        f_rendered.contains("Integer") && f_rendered.contains("Bool"),
        "expected f : Integer → F Bool, got: {f_rendered}"
    );
    assert!(
        g_rendered.contains("String") && g_rendered.contains("Integer"),
        "expected g : String → F Integer, got: {g_rendered}"
    );
}

/// A partial application's RHS resolves to `Fun`, so `annotate` η-expands it:
/// `let g = f 1` becomes `Return(Thunk(Lam x. App { f, [1, x] }))`,
/// whose body `Comp::arrow` reads as a `Lam`, and `g`'s scheme is
/// generalised over the still-free second parameter.
#[test]
fn toplevel_partial_application_eta_expands_to_thunked_lambda() {
    let top = toplevel_ok(
        "let f = { |a b| return $a }\n\
         let g = f 1\n\
         return ()",
    );
    let Phrase::Define { comp, schemes, .. } = &top.phrases[1].item else {
        panic!("expected g's Define phrase, got {:?}", top.phrases[1].item);
    };
    let CompKind::Return(Val::Thunk(body)) = &comp.item else {
        panic!("expected Return(Thunk(..)), got {:?}", comp.item);
    };
    let (param, lam_body) = body
        .shape()
        .arrow()
        .expect("g's thunk body must be a syntactic Lam");
    assert!(matches!(param, Pattern::Name(_)));
    let CompKind::App { args, .. } = &lam_body.item else {
        panic!("expected an App body, got {:?}", lam_body.item);
    };
    assert_eq!(
        args.len(),
        2,
        "expected the original argument plus the eta parameter"
    );

    assert_eq!(schemes.len(), 1);
    let rendered = schemes[0].1.to_string();
    assert!(
        rendered.starts_with('∀') && rendered.contains('→'),
        "expected g : U (B → C), got: {rendered}"
    );
}

// ─── Computed indexes: one relation, settled at the unit's end ─────────────

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

fn codes(src: &str) -> Vec<&'static str> {
    raw_errors(src).iter().map(|e| e.kind.code()).collect()
}

#[test]
fn a_computed_index_nothing_decides_is_refused_at_the_units_end() {
    let src = "let pick = { |m k| $m[$k] }\nreturn ()";
    assert_eq!(codes(src), ["T0075"]);
    has_error(src, "is `$m` a list or a map?");
}

#[test]
fn a_computed_index_helper_is_accepted_at_the_one_container_type_its_unit_uses() {
    let top = toplevel_ok(
        "let pick = { |m k| $m[$k] }\n\
         let a = pick [\"x\": 1] 'x'\n\
         let b = pick [\"y\": 2] 'y'\n\
         return ()",
    );
    assert_eq!(scheme_at(&top, 0), "Map Integer → String → Returns Integer");
}

#[test]
fn a_computed_index_helper_used_on_a_list_and_a_map_is_a_mismatch() {
    let src = "let pick = { |m k| $m[$k] }\n\
               let a = pick [1, 2] 0\n\
               let b = pick [\"y\": 2] 'y'\n\
               return ()";
    let errs = raw_errors(src);
    assert_eq!(codes(src), ["T0010"], "{errs:?}");
    let note = errs[0].hint().expect("a mismatch on a weak type is noted");
    assert!(note.contains("shared by every use"), "{note}");
}

#[test]
fn a_literal_key_is_the_list_rule_and_stays_generic() {
    let top = toplevel_ok(
        "let first = { |xs| $xs[0] }\n\
         let a = first [1, 2]\n\
         let b = first ['x']\n\
         return ()",
    );
    assert_eq!(scheme_at(&top, 0), "∀α. [α] → Returns α");
}

#[test]
fn a_computed_key_makes_the_helper_monomorphic_for_the_unit() {
    let src = "let nth = { |xs i| $xs[$i] }\n\
               let a = nth [1, 2] 0\n\
               let b = nth ['x'] 0\n\
               return ()";
    assert_eq!(codes(src), ["T0010"]);
    // Its own body fixing the key changes nothing: cost 19.
    let src = "let nth = { |xs i| let j = $[$i + 1]; $xs[$j] }\n\
               let a = nth [1, 2] 0\n\
               let b = nth ['x'] 0\n\
               return ()";
    assert_eq!(codes(src), ["T0010"]);
}

/// Whether a helper is generic must not depend on where an independent
/// `let` stands.
#[test]
fn the_verdict_on_an_index_helper_is_independent_of_statement_order() {
    let body = |n_first: bool, second: &str| {
        let n = "let n = $[$i + 1]\n";
        let h = "let h = { |c| $c[$i] }\n";
        let (first, then) = if n_first { (n, h) } else { (h, n) };
        format!("let run = {{ |i|\n{first}{then}h [1]\nh {second}\n}}\nreturn ()")
    };
    for n_first in [true, false] {
        let refused = body(n_first, "['x']");
        assert_eq!(codes(&refused), ["T0010"], "n first: {n_first}");
        assert!(
            errors(&body(n_first, "[2]")).is_empty(),
            "n first: {n_first}"
        );
    }
    let schemes: Vec<String> = [true, false]
        .map(|n_first| scheme_at(&toplevel_ok(&body(n_first, "[2]")), 0))
        .into();
    assert_eq!(schemes[0], schemes[1]);
}

/// A helper that indexes is not generic in its container: an Int is refused.
#[test]
fn a_computed_index_on_an_integer_is_refused() {
    let src = "let pick = { |m k| $m[$k] }\nlet x = pick 5 5\nreturn ()";
    assert_eq!(codes(src), ["T0062"]);
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
fn a_key_of_number_kind_settles_the_index_as_a_list() {
    let top = toplevel_ok("let f = { |xs k| let j = $[$k + 1]; $xs[$k] }\nreturn ()");
    assert_eq!(scheme_at(&top, 0), "[_α] → Integer → Returns _α");
}

#[test]
fn a_target_of_comparable_kind_is_refused_with_the_kind_sentence() {
    let src = "let f = { |c d k| let b = $[$c < $d]; $c[$k] }\nreturn ()";
    assert_eq!(codes(src), ["T0074"]);
    has_error(src, "nothing is both");
}

#[test]
fn a_key_that_is_neither_an_int_nor_a_string_is_refused() {
    let src = "let k = $[1 < 2]\nlet f = { |m| $m[$k] }\nreturn ()";
    assert_eq!(codes(src), ["T0074"]);
    has_error(
        src,
        "but an Int key for a list or a String key for a map is needed",
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
    assert!(!errs.is_empty(), "expected a schema error, got none");
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
    assert!(
        !errs.is_empty(),
        "a computed return carries a row, and the row is checked"
    );
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
/// the runtime doors use.
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
        let errs = contract_errors(declared(form), "return [\"nn\": 1]");
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

/// A return typed at a variable is the runtime door's.
#[test]
fn return_schema_leaves_a_decoded_value_to_the_runtime_door() {
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
// `grant` holds `net`, `editor` and `shell` to closed variants where they are
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

/// Specific advice replaces the nearest-tag guess, which would contradict it.
#[test]
fn grant_advice_replaces_the_nearest_tag_guess() {
    let msgs = grant_messages("grant [exec: [allow: [`dir `name git]]] { return 1 }");
    assert!(!msgs[0].contains("did you mean"), "{msgs:?}");
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
        let bound = grant_messages(&format!(
            "let t = {table}\ngrant [{axis}: $t] {{ return 1 }}"
        ));
        assert_eq!(bound.len(), 1, "bound {table}: {bound:?}");
        let profile = contract_errors(declared(Form::Grant), &format!("return [{axis}: {table}]"));
        assert_eq!(profile.len(), 1, "profile {table}: {profile:?}");
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
    for table in ["[:]", "[\"allow\": []]", "()"] {
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

/// Compile `src` to an annotated toplevel, asserting it type-checks.
fn annotated(src: &str) -> Toplevel {
    toplevel_ok(src)
}

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
    walk_toplevel(&annotated(src), &mut |c| {
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
