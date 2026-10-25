//! Stored schemes are re-seeded without sharing foreign unifier ids.

use std::sync::Arc;

use super::SessionSchemes;
use crate::compile::{CompileError, compile_and_typecheck};
use crate::source::FileId;
use crate::ty::{Kind, Scheme, Ty, TyVar, WeakVars};

fn check_with_stored(names: &[&str], src: &str) -> Vec<super::TypeError> {
    let elem = TyVar(0);
    let stored = Scheme {
        weak: WeakVars {
            tys: [(elem, Kind::ANY)].into(),
            ..WeakVars::default()
        },
        ..Scheme::mono(Ty::list(Ty::Var(elem)))
    };
    let schemes = SessionSchemes {
        bindings: names
            .iter()
            .map(|name| ((*name).to_string(), Some(Arc::new(stored.clone()))))
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
