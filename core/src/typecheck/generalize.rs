//! Generalization and instantiation for HM polymorphism.
//!
//! `generalize` closes over the free type/mode/row variables in a type that
//! are not mentioned in the ambient environment, producing a ∀-quantified
//! scheme.  `instantiate` opens a scheme by replacing each quantified variable
//! with a fresh unification variable.

use super::env::{FreeVars, TyEnv, env_free_vars, free_ty};
use super::scheme::{CachedFreeVars, Scheme};
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
use super::unify::Unifier;
use std::collections::HashMap;

pub fn generalize(u: &mut Unifier, env: &TyEnv, ty: &Ty) -> Scheme {
    let applied = u.apply_ty(ty);

    let mut fvs = FreeVars::new();
    free_ty(u, &applied, &mut fvs);
    let env_fvs = env_free_vars(u, env);

    let ty_vars: Vec<TyVar> = fvs.tys.difference(&env_fvs.tys).copied().collect();
    let comp_ty_vars: Vec<CompTyVar> = fvs.comps.difference(&env_fvs.comps).copied().collect();
    let mode_vars: Vec<ModeVar> = fvs.modes.difference(&env_fvs.modes).copied().collect();
    let row_vars: Vec<RowVar> = fvs.rows.difference(&env_fvs.rows).copied().collect();

    // Cache the residual free variables: those that appear in the environment
    // and were therefore NOT generalised.  For top-level bindings these are
    // all empty.  Stored so that future env_free_vars calls can skip traversal
    // for this scheme instead of re-walking the type tree.
    let cached_fv = Some(CachedFreeVars {
        ty_fv: fvs.tys.intersection(&env_fvs.tys).copied().collect(),
        comp_fv: fvs.comps.intersection(&env_fvs.comps).copied().collect(),
        mode_fv: fvs.modes.intersection(&env_fvs.modes).copied().collect(),
        row_fv: fvs.rows.intersection(&env_fvs.rows).copied().collect(),
    });

    // Snapshot any cyclic comp-var bindings reachable from `applied`.
    // The cycle-aware `apply_*` chain leaves comp-var back-edges as
    // `CompTy::Var(root)` nodes; collecting those roots and their
    // bindings lets `instantiate` mint fresh ids without sharing the
    // original union-find slot across instantiations.
    let comp_ty_bindings = snapshot_cyclic_comp_bindings(u, &applied);

    // Cyclic roots already appear in `comp_ty_bindings` — drop them
    // from the plain `comp_ty_vars` set so they are not double-counted.
    let cyclic_roots: std::collections::HashSet<u32> =
        comp_ty_bindings.iter().map(|(r, _)| *r).collect();
    let comp_ty_vars: Vec<CompTyVar> = comp_ty_vars
        .into_iter()
        .filter(|v| !cyclic_roots.contains(&v.0))
        .collect();

    Scheme {
        ty_vars,
        comp_ty_vars,
        mode_vars,
        row_vars,
        ty: applied,
        comp_ty_bindings,
        cached_fv,
    }
}

pub fn instantiate(u: &mut Unifier, scheme: &Scheme) -> Ty {
    if !scheme.is_poly() {
        return scheme.ty.clone();
    }
    // Build a single comp-var rename map covering both the
    // non-cyclic quantified set and the cyclic-binding roots.  Mints
    // a fresh union-find root per old id so two instantiations never
    // share state.
    let mut cm: HashMap<u32, u32> = HashMap::new();
    for v in &scheme.comp_ty_vars {
        cm.insert(v.0, u.fresh_comp_root());
    }
    for (old, _) in &scheme.comp_ty_bindings {
        cm.insert(*old, u.fresh_comp_root());
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
        cm: cm.clone(),
    };
    // Re-bind each fresh cyclic-comp-var root to the substituted binding
    // so the cycle survives instantiation but lives in fresh union-find
    // slots.  Non-cyclic vars are left as fresh free roots.
    for (old, binding) in &scheme.comp_ty_bindings {
        let fresh_root = cm[old];
        let substituted = sm.comp(binding);
        u.bind_comp_root(fresh_root, substituted);
    }
    sm.ty(&scheme.ty)
}

/// Walk an applied type and collect the comp-var roots that appear as
/// `CompTy::Var(_)` back-edges, paired with each root's resolved binding
/// from the unifier.  Bindings are themselves applied (cycle-aware) so
/// that re-binding fresh roots to them at instantiation time produces a
/// detached copy of the cyclic structure.
fn snapshot_cyclic_comp_bindings(u: &mut Unifier, applied: &Ty) -> Vec<(u32, CompTy)> {
    u
        .cyclic_comp_roots_in_ty(applied)
        .into_iter()
        .map(|root| {
            let binding = u
                .resolved_comp_root_binding(root)
                .unwrap_or(CompTy::Var(CompTyVar(root)));
            (root, binding)
        })
        .collect()
}

/// Simultaneous substitution of type, mode, row, and comp-ty variables.
/// `cm` carries the mapping from old cyclic comp-var roots to fresh
/// ones — empty for non-recursive schemes.
struct SubstMap {
    tm: HashMap<TyVar, TyVar>,
    mm: HashMap<ModeVar, ModeVar>,
    rm: HashMap<RowVar, RowVar>,
    cm: HashMap<u32, u32>,
}

impl SubstMap {
    fn ty(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(v) => self.tm.get(v).map_or_else(|| ty.clone(), |&f| Ty::Var(f)),
            Ty::List(a) => Ty::List(Box::new(self.ty(a))),
            Ty::Map(a) => Ty::Map(Box::new(self.ty(a))),
            Ty::Handle(a) => Ty::Handle(Box::new(self.ty(a))),
            Ty::Record(r) => Ty::Record(self.row(r)),
            Ty::Variant(r) => Ty::Variant(self.row(r)),
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
            CompTy::Var(CompTyVar(i)) => {
                // Cyclic-comp back-edge: rewrite to the freshly minted
                // root id when the scheme captured this one as cyclic.
                let id = *self.cm.get(i).unwrap_or(i);
                CompTy::Var(CompTyVar(id))
            }
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
            PipeMode::Var(v) => self
                .mm
                .get(v)
                .map_or_else(|| mode.clone(), |&f| PipeMode::Var(f)),
        }
    }
}
