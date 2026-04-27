//! Generalization and instantiation for HM polymorphism.
//!
//! `generalize` closes over the free type/mode/row variables in a type that
//! are not mentioned in the ambient environment, producing a ∀-quantified
//! scheme.  `instantiate` opens a scheme by replacing each quantified variable
//! with a fresh unification variable.

use super::env::{FreeVars, TyEnv, env_free_vars, free_ty};
use super::scheme::{CachedFreeVars, Scheme};
use super::ty::{CompTy, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
use super::unify::Unifier;
use std::collections::HashMap;

pub fn generalize(u: &mut Unifier, env: &TyEnv, ty: &Ty) -> Scheme {
    let applied = u.apply_ty(ty);

    let mut fvs = FreeVars::new();
    free_ty(u, &applied, &mut fvs);
    let env_fvs = env_free_vars(u, env);

    let ty_vars: Vec<TyVar> = fvs.tys.difference(&env_fvs.tys).copied().collect();
    let mode_vars: Vec<ModeVar> = fvs.modes.difference(&env_fvs.modes).copied().collect();
    let row_vars: Vec<RowVar> = fvs.rows.difference(&env_fvs.rows).copied().collect();

    // Cache the residual free variables: those that appear in the environment
    // and were therefore NOT generalised.  For top-level bindings these are
    // all empty.  Stored so that future env_free_vars calls can skip traversal
    // for this scheme instead of re-walking the type tree.
    let cached_fv = Some(CachedFreeVars {
        ty_fv: fvs.tys.intersection(&env_fvs.tys).copied().collect(),
        mode_fv: fvs.modes.intersection(&env_fvs.modes).copied().collect(),
        row_fv: fvs.rows.intersection(&env_fvs.rows).copied().collect(),
    });

    Scheme {
        ty_vars,
        mode_vars,
        row_vars,
        ty: applied,
        cached_fv,
    }
}

pub fn instantiate(u: &mut Unifier, scheme: &Scheme) -> Ty {
    if !scheme.is_poly() {
        return scheme.ty.clone();
    }
    let sm = SubstMap {
        tm: scheme
            .ty_vars
            .iter()
            .map(|&v| (v, u.fresh_tyvar()))
            .collect(),
        mm: scheme
            .mode_vars
            .iter()
            .map(|&v| (v, u.fresh_modevar()))
            .collect(),
        rm: scheme
            .row_vars
            .iter()
            .map(|&v| (v, u.fresh_row_var()))
            .collect(),
    };
    sm.ty(&scheme.ty)
}

/// Simultaneous substitution of type, mode, and row variables.
struct SubstMap {
    tm: HashMap<TyVar, TyVar>,
    mm: HashMap<ModeVar, ModeVar>,
    rm: HashMap<RowVar, RowVar>,
}

impl SubstMap {
    fn ty(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(v) => self.tm.get(v).map_or_else(|| ty.clone(), |&f| Ty::Var(f)),
            Ty::List(a) => Ty::List(Box::new(self.ty(a))),
            Ty::Map(a) => Ty::Map(Box::new(self.ty(a))),
            Ty::Handle(a) => Ty::Handle(Box::new(self.ty(a))),
            Ty::Record(r) => Ty::Record(self.row(r)),
            Ty::Thunk(b) => Ty::Thunk(Box::new(self.comp(b))),
            _ => ty.clone(),
        }
    }

    fn row(&self, row: &Row) -> Row {
        match row {
            Row::Empty => Row::Empty,
            Row::Var(v) => self.rm.get(v).map_or_else(|| row.clone(), |&f| Row::Var(f)),
            Row::Extend(l, ty, rest) => {
                Row::Extend(l.clone(), Box::new(self.ty(ty)), Box::new(self.row(rest)))
            }
        }
    }

    fn comp(&self, cty: &CompTy) -> CompTy {
        match cty {
            CompTy::Var(i) => CompTy::Var(*i),
            CompTy::Return(spec, a) => CompTy::Return(
                PipeSpec {
                    input: self.mode(&spec.input),
                    output: self.mode(&spec.output),
                },
                Box::new(self.ty(a)),
            ),
            CompTy::Fun(a, b) => CompTy::Fun(Box::new(self.ty(a)), Box::new(self.comp(b))),
        }
    }

    fn mode(&self, mode: &PipeMode) -> PipeMode {
        match mode {
            PipeMode::None | PipeMode::Bytes => mode.clone(),
            PipeMode::Values(a) => PipeMode::Values(Box::new(self.ty(a))),
            PipeMode::Var(v) => self
                .mm
                .get(v)
                .map_or_else(|| mode.clone(), |&f| PipeMode::Var(f)),
        }
    }
}
