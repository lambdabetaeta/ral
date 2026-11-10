//! Stored schemes are re-seeded without sharing foreign unifier ids.

use std::sync::Arc;

use super::SessionSchemes;
use crate::compile::{CompileError, compile_and_typecheck};
use crate::source::FileId;
use crate::ty::{Binders, Closed, Kind, Role, Scheme, Ty, TyVar};

/// `scheme` closed as a session would store it; its weak variables are residuals.
fn stored(scheme: Scheme) -> Closed {
    Closed::new(Arc::new(scheme)).unwrap()
}

fn check_with_stored(names: &[&str], src: &str) -> Vec<super::TypeError> {
    let elem = TyVar(0);
    let stored = stored(Scheme {
        tys: [(elem, Role::Weak(Kind::ANY))].into(),
        ..Scheme::mono(Ty::list(Ty::var(elem)))
    });
    let schemes = SessionSchemes {
        bindings: names
            .iter()
            .map(|name| ((*name).to_string(), Some(stored.clone())))
            .collect(),
        ..crate::test_helper::core_schemes()
    };
    match compile_and_typecheck(src, schemes, FileId::DUMMY, None, None) {
        Ok(_) => Vec::new(),
        Err(CompileError::Parse(e)) => panic!("parse: {src:?}: {e}"),
        Err(CompileError::Types(errs)) => errs,
    }
}

const ELEMENT_AS_INT_THEN_BOOL: &str = "
let [a] = $xs
let n = $[$a + 1]
let [b] = $ys
let m = $[not $b]";

/// Two names stored by different units carry the same residual number. Each
/// is re-seeded as its own fresh weak variable.
#[test]
fn stored_residuals_do_not_alias_across_names() {
    let errs = check_with_stored(&["xs", "ys"], ELEMENT_AS_INT_THEN_BOOL);
    assert!(
        errs.is_empty(),
        "residuals of two units must stay apart, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_stored_residual_has_one_type_per_unit() {
    let errs = check_with_stored(&["xs"], &ELEMENT_AS_INT_THEN_BOOL.replace("$ys", "$xs"));
    assert!(
        errs.iter()
            .any(|e| e.hint().is_some_and(|h| h.contains("another use fixed it"))),
        "a weak residual must not be instantiated at a number and at a Bool, and the \
         refusal says why: {errs:?}"
    );
}

fn quantified_tys(scheme: &Scheme) -> usize {
    scheme
        .tys
        .values()
        .filter(|role| role.forall().is_some())
        .count()
}

fn identity_body() -> Arc<crate::ir::Comp> {
    use crate::ir::{CompKind, Val};
    use crate::source::Spanned;
    Arc::new(Spanned::synthetic(CompKind::Lam {
        param: "x".into(),
        body: Arc::new(Spanned::synthetic(CompKind::Return(Val::Variable(
            "x".into(),
        )))),
    }))
}

fn identity_ty(param: Ty) -> Ty {
    Ty::thunk(crate::ty::CompTy::arrows(
        [param.clone()],
        crate::ty::CompTy::pure(param),
    ))
}

#[test]
fn binding_value_scheme_quantifies_its_identity_parameter() {
    let scheme =
        super::binding_value_scheme(&identity_body(), crate::test_helper::core_schemes()).unwrap();
    assert_eq!(quantified_tys(&scheme), 1);
    assert!(!scheme.has_weak());
    let mut u = super::Unifier::new();
    for param in [Ty::int(), Ty::bool()] {
        let instance = super::instantiate(&mut u, &scheme);
        u.unify_ty(&instance, &identity_ty(param)).unwrap();
    }
}

#[test]
fn alias_arm_scheme_retains_its_polymorphic_result() {
    use crate::ir::{CompKind, Val};
    use crate::source::Spanned;
    use crate::ty::CompTy;
    let head = Binders::close(|binders| {
        let variable = Ty::var(binders.ty(Kind::ANY));
        Ty::thunk(CompTy::arrows(
            [Ty::argv()],
            CompTy::pure(identity_ty(variable)),
        ))
    });
    let schemes = SessionSchemes {
        aliases: vec![("identity-arm".into(), head)],
        ..crate::test_helper::core_schemes()
    };
    let arm = Arc::new(Spanned::synthetic(CompKind::Lam {
        param: "args".into(),
        body: Arc::new(Spanned::synthetic(CompKind::Return(Val::thunk(
            identity_body(),
        )))),
    }));
    let scheme = super::alias_arm_scheme("identity-arm", &arm, schemes).unwrap();
    assert_eq!(quantified_tys(&scheme), 1);
    assert!(!scheme.has_weak());
    let mut u = super::Unifier::new();
    for param in [Ty::int(), Ty::bool()] {
        let instance = super::instantiate(&mut u, &scheme);
        let wanted = Ty::thunk(CompTy::arrows(
            [Ty::argv()],
            CompTy::pure(identity_ty(param)),
        ));
        u.unify_ty(&instance, &wanted).unwrap();
    }
}

#[test]
fn catch_all_stands_in_checks_its_body_in_a_one_shot_region() {
    use crate::ir::{CompKind, Val};
    use crate::source::Spanned;
    let arm = Arc::new(Spanned::synthetic(CompKind::Lam {
        param: "name".into(),
        body: Arc::new(Spanned::synthetic(CompKind::Lam {
            param: "args".into(),
            body: Arc::new(Spanned::synthetic(CompKind::Return(Val::Unit))),
        })),
    }));
    super::catch_all_stands_in(&arm, crate::test_helper::core_schemes()).unwrap();
}

fn command_with_unit_argument(params: &[&str]) -> Arc<crate::ir::Comp> {
    use crate::ir::{CommandName, CommandWord, CompKind, Exec, Val, ValListElem};
    use crate::source::Spanned;
    let mut body = Arc::new(Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::External(CommandName::Bare("echo".into())),
        argv: vec![ValListElem::Single(Spanned::synthetic(Val::Unit))].into(),
        redirects: crate::ir::Redirects::default(),
        site: None,
    })));
    for param in params.iter().rev() {
        body = Arc::new(Spanned::synthetic(CompKind::Lam {
            param: (*param).into(),
            body,
        }));
    }
    body
}

#[test]
fn closure_only_gate_refusal_exports_no_one_shot_scheme_or_checked_tree() {
    use crate::ir::{Pattern, Phrase};
    use crate::source::Spanned;
    let body = command_with_unit_argument(&["args"]);
    let binding = super::binding_value_scheme(&body, crate::test_helper::core_schemes())
        .expect_err("the late argv gate must prevent scheme export");
    let alias = super::alias_arm_scheme("echo", &body, crate::test_helper::core_schemes())
        .expect_err("the late argv gate must prevent alias scheme export");
    let catch_all = super::catch_all_stands_in(
        &command_with_unit_argument(&["name", "args"]),
        crate::test_helper::core_schemes(),
    )
    .expect_err("the late argv gate must refuse a catch-all");
    for error in [binding, alias, catch_all] {
        assert_eq!(error.kind.code(), "T0057");
    }
    let errors = super::typecheck(
        &[Spanned::synthetic(Phrase::Define {
            pattern: Arc::new(Pattern::Name("f".into())),
            comp: body,
            hole: (),
            schemes: (),
        })],
        crate::test_helper::core_schemes(),
        None,
    )
    .expect_err("the late argv gate must prevent readback");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind.code(), "T0057");
}

#[test]
fn takes_reseeds_a_stored_schemes_weak_payload_before_instantiation() {
    use crate::ty::{CompTy, Grade, GradeVar};

    let residual = TyVar(500);
    let grade = GradeVar(700);
    let scheme = stored(Scheme {
        tys: [(residual, Role::Weak(Kind::ANY))].into(),
        grades: [grade].into(),
        ..Scheme::mono(Ty::thunk(CompTy::arrows(
            [Ty::int()],
            CompTy::ret(Grade::Var(grade), Ty::list(Ty::var(residual))),
        )))
    });
    assert!(super::takes(&scheme, 1));
    assert!(!super::takes(&scheme, 0));
    assert!(!super::takes(&scheme, 2));
}
