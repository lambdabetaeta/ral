//! Behavioural oracle for the HM static type checker: what the checker
//! computes and annotates, read off the checker's own data.
//!
//! Every test parses + elaborates a small ral program and runs `typecheck()`.
//! What the checker *refuses*, and the words it refuses in, lives in the
//! corpus of programs under `tests/reject/` and `tests/accept/`, run by
//! `ral/tests/corpus.rs`.

mod common;

use ral_core::typecheck::{CompTy, CompTyVar, Ty, fmt_scheme};
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
        None,
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
    let rendered = fmt_scheme(&scheme);
    assert_eq!(rendered, "∀ϕ. ϕ");
}

#[test]
fn fmt_scheme_quantifies_cyclic_comp_roots() {
    let root = CompTyVar(29);
    let scheme = ral_core::test_access::scheme_over_comp_vars(
        vec![],
        Ty::Thunk(Box::new(CompTy::Var(root))),
        vec![(root.0, CompTy::pure(Ty::Unit))],
    );
    let rendered = fmt_scheme(&scheme);
    assert_eq!(rendered, "∀ϕ. ϕ");
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

// ─── IR route annotation ──────────────────────────────────────────────────────
//
// The annotation pass writes the checker's ground verdicts into a rebuilt IR
// as explicit syntax: one `PipeYield` per pipeline, and a `Capture` node
// wherever a value boundary meets a byte route.  Schemes stay on the
// top-level spine only; the yields and the captures go everywhere, at any
// depth.

use ral_core::ir::{Comp, CompKind, IrPattern, Phrase, PipeYield, Toplevel, Val};

/// Compile `src` to an annotated toplevel, asserting it type-checks.
fn annotated(src: &str) -> Toplevel {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    let top = elaborate(&ast, std::collections::HashSet::default(), "")
        .unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
    typecheck(
        &top,
        ral_core::SessionSchemes::from_schemes(
            common::prelude_schemes(),
            ral_core::HostSurface::default().builtin_table(),
        ),
        None,
    )
    .unwrap_or_else(|errs| panic!("expected no errors in {src:?}, got: {errs:?}"))
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

/// Every `Pipeline` node's stage count and yield, anywhere in the tree.
fn all_pipeline_yields(top: &Toplevel) -> Vec<(usize, PipeYield)> {
    let mut out = Vec::new();
    walk_toplevel(top, &mut |c| {
        if let CompKind::Pipeline { stages, yields, .. } = &c.item {
            out.push((stages.len(), *yields));
        }
    });
    out
}

/// Every `Pipeline` node's `stage_types` slot, reachable anywhere.
fn all_pipeline_stage_types(top: &Toplevel) -> Vec<(usize, Vec<Ty>)> {
    let mut out = Vec::new();
    walk_toplevel(top, &mut |c| {
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
    // Each stage's value type is `Unit`, its bytes being the `result`.
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

/// Whether the `x` bind of `src` had its RHS captured — the whole observable
/// content of a `Bytes` route at a value boundary.
fn bind_x_is_captured(src: &str) -> bool {
    let top = annotated(src);

    // A join's byte-side arms are captured individually (per-arm, not at
    // the join's own node), so the verdict is "any Capture in the RHS
    // subtree", not "the RHS itself is one".
    fn has_capture(rhs: &Comp) -> bool {
        let mut found = false;
        common::walk_comp(rhs, &mut |c| {
            if let CompKind::Capture(_) = &c.item {
                found = true;
            }
        });
        found
    }

    let mut found = None;
    // `x` bound at the top level is a `Phrase::Define`; nested under a
    // block it is a `CompKind::Bind`, reached by `walk_toplevel`.
    for phrase in &top.phrases {
        if let Phrase::Define { pattern, comp, .. } = &phrase.item
            && let IrPattern::Name(name) = pattern.as_ref()
            && name.as_ref() == "x"
        {
            found = Some(has_capture(comp));
        }
    }
    walk_toplevel(&top, &mut |c| {
        if let CompKind::Bind {
            comp: rhs, pattern, ..
        } = &c.item
            && let IrPattern::Name(name) = pattern.as_ref()
            && name.as_ref() == "x"
        {
            found = Some(has_capture(rhs));
        }
    });
    found.expect("a bind named `x`")
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

// ─── Toplevel phrase typechecking ─────────────────────────────────────────

fn toplevel(src: &str) -> Toplevel {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    elaborate(&ast, std::collections::HashSet::default(), "")
        .unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"))
}

fn toplevel_ok(src: &str) -> Toplevel {
    typecheck(
        &toplevel(src),
        ral_core::SessionSchemes::from_schemes(
            common::prelude_schemes(),
            ral_core::HostSurface::default().builtin_table(),
        ),
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
    let f_rendered = fmt_scheme(&f_schemes[0].1);
    let g_rendered = fmt_scheme(&g_schemes[0].1);
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
    assert!(matches!(param, IrPattern::Name(_)));
    let CompKind::App { args, .. } = &lam_body.item else {
        panic!("expected an App body, got {:?}", lam_body.item);
    };
    assert_eq!(
        args.len(),
        2,
        "expected the original argument plus the eta parameter"
    );

    assert_eq!(schemes.len(), 1);
    let rendered = fmt_scheme(&schemes[0].1);
    assert!(
        rendered.starts_with('∀') && rendered.contains('→'),
        "expected g : U (B → C), got: {rendered}"
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
};

fn contract_errors(table: &'static ral_core::typecheck::Table, src: &str) -> Vec<TypeError> {
    let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
    let top = elaborate(&ast, std::collections::HashSet::default(), "")
        .unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
    typecheck(
        &top,
        ral_core::SessionSchemes::from_schemes(
            common::prelude_schemes(),
            ral_core::HostSurface::default().builtin_table(),
        ),
        Some(table),
    )
    .err()
    .unwrap_or_default()
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
    let errs = schema_errors("let extra = [nn: 1]\nreturn [...$extra, loose: 1]");
    assert!(
        errs.iter().any(|e| e.kind.code() == "T0020"),
        "expected T0020, got: {errs:?}"
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

/// A return with no row — the empty `[:]` — has nothing to check, and stays
/// on the runtime door that dispatches off the same table.
#[test]
fn return_schema_leaves_a_map_to_the_runtime_door() {
    let errs = schema_errors("return [:, nn: 1]");
    assert!(errs.is_empty(), "a map carries no row, got: {errs:?}");
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
