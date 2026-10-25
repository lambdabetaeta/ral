//! Generalization and instantiation for HM polymorphism.
//!
//! Equi-recursion is the twist: a cyclic type is anchored at a union-find
//! root, so a scheme snapshots that root's binding and `instantiate`
//! re-anchors it at a fresh root instead of sharing the original slot.

use super::env::TyEnv;
use super::unify::{Unifier, Var, Visited};
use crate::ty::{
    CompTy, CompTyVar, Grade, GradeVar, Kind, Row, RowVar, Scheme, Term, Ty, TyVar, WeakVars,
};
use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::ops::ControlFlow;
use std::sync::Arc;

/// All four variable kinds, collected in one traversal.  Ordered, so a
/// scheme — and the letters it prints under — is a function of the type.
#[derive(Clone)]
pub(crate) struct FreeVars {
    pub(crate) tys: BTreeSet<TyVar>,
    pub(crate) comps: BTreeSet<CompTyVar>,
    pub(crate) rows: BTreeSet<RowVar>,
    pub(crate) grades: BTreeSet<GradeVar>,
}

impl FreeVars {
    pub fn new() -> Self {
        Self {
            tys: BTreeSet::new(),
            comps: BTreeSet::new(),
            rows: BTreeSet::new(),
            grades: BTreeSet::new(),
        }
    }

    /// A variable met on a walk, at its root.
    fn insert(&mut self, var: Var) {
        match var {
            Var::Ty(v) => self.tys.insert(v),
            Var::Comp(v) => self.comps.insert(v),
            Var::Row(v) => self.rows.insert(v),
            Var::Grade(v) => self.grades.insert(v),
        };
    }

    pub(crate) fn merge_into(self, target: &mut Self) {
        target.tys.extend(self.tys);
        target.comps.extend(self.comps);
        target.rows.extend(self.rows);
        target.grades.extend(self.grades);
    }

    /// The weak variables among these.
    pub(crate) fn weak(&self, u: &Unifier) -> WeakVars {
        WeakVars {
            tys: self
                .tys
                .iter()
                .copied()
                .filter(|&v| u.is_weak_ty(v))
                .map(|v| (v, u.kinded(v).kind))
                .collect(),
            comps: self
                .comps
                .iter()
                .copied()
                .filter(|&v| u.is_weak_comp(v))
                .collect(),
            rows: self
                .rows
                .iter()
                .copied()
                .filter(|&v| u.is_weak_row(v))
                .map(|v| (v, u.is_deep_row(v)))
                .collect(),
        }
    }

    pub(crate) fn remove_weak(&mut self, weak: &WeakVars) {
        self.tys.retain(|v| !weak.tys.contains_key(v));
        self.comps.retain(|v| !weak.comps.contains(v));
        self.rows.retain(|v| !weak.rows.contains_key(v));
    }

    /// Instantiation mints these fresh, so they are not free in the environment.
    pub(crate) fn remove_quantified(&mut self, s: &Scheme) {
        for (v, _) in &s.ty_vars {
            self.tys.remove(v);
        }
        for v in &s.comp_ty_vars {
            self.comps.remove(v);
        }
        for (v, _) in &s.row_vars {
            self.rows.remove(v);
        }
        for v in &s.grade_vars {
            self.grades.remove(v);
        }
    }

    /// The *residual* free vars — mentioned by `env`, so left unquantified.
    pub(crate) fn intersect_into_cached(&self, env: &Self) -> crate::ty::CachedFreeVars {
        crate::ty::CachedFreeVars {
            ty_fv: self.tys.intersection(&env.tys).copied().collect(),
            comp_fv: self.comps.intersection(&env.comps).copied().collect(),
            row_fv: self.rows.intersection(&env.rows).copied().collect(),
            grade_fv: self.grades.intersection(&env.grades).copied().collect(),
        }
    }
}

pub(crate) fn free_ty(u: &Unifier, ty: &Ty, out: &mut FreeVars) {
    let mut collect = |var: Var| {
        if u.is_unbound(var) {
            out.insert(var);
        }
        ControlFlow::<Infallible>::Continue(())
    };
    let _ = u.reach(Term::Ty(ty), &mut Visited::default(), &mut collect);
}

pub(crate) fn env_free_vars(u: &Unifier, env: &TyEnv) -> FreeVars {
    let mut out = FreeVars::new();
    for s in env.all_schemes() {
        if let Some(cached) = &s.cached_fv {
            for &v in &cached.ty_fv {
                free_ty(u, &Ty::Var(v), &mut out);
            }
            for &v in &cached.comp_fv {
                free_ty(u, &Ty::Thunk(Box::new(CompTy::Var(v))), &mut out);
            }
            for &v in &cached.row_fv {
                free_ty(u, &Ty::Record(Row::Var(v)), &mut out);
            }
            for &v in &cached.grade_fv {
                if let Grade::Var(root) = u.resolve_grade(Grade::Var(v)) {
                    out.grades.insert(root);
                }
            }
        } else {
            let mut fvs = FreeVars::new();
            free_ty(u, &s.ty, &mut fvs);
            fvs.remove_quantified(s);
            fvs.merge_into(&mut out);
        }
    }
    out
}

pub(crate) fn generalize(u: &Unifier, env: &TyEnv, tied: &FreeVars, ty: &Ty) -> Scheme {
    let applied = u.apply_ty(ty);

    let mut fvs = FreeVars::new();
    free_ty(u, &applied, &mut fvs);
    let weak = fvs.weak(u);
    fvs.remove_weak(&weak);
    let mut env_fvs = env_free_vars(u, env);
    tied.clone().merge_into(&mut env_fvs);

    let ty_vars = fvs.tys.difference(&env_fvs.tys).copied();
    let comp_ty_vars = fvs.comps.difference(&env_fvs.comps).copied();
    let row_vars: Vec<(RowVar, bool)> = fvs
        .rows
        .difference(&env_fvs.rows)
        .map(|&v| (v, u.is_deep_row(v)))
        .collect();
    let grade_vars: Vec<GradeVar> = fvs.grades.difference(&env_fvs.grades).copied().collect();

    // Cache residual seeds, not their evolving roots or the whole type tree.
    let cached_fv = Some(fvs.intersect_into_cached(&env_fvs));

    // `apply_*` leaves each cycle as a `Var(root)` back-edge; snapshotting root
    // and binding lets `instantiate` rebuild the cycle in a fresh slot.  Roots
    // free in the env are excluded, as from the quantifier sets above: they are
    // monomorphic and must stay anchored so one use's constraint reaches all.
    // Each binding is itself applied, cycle-aware: re-binding a fresh root in
    // `instantiate` detaches the copy.
    let (comp_ty_bindings, ty_bindings) = cyclic_bindings(
        u,
        &applied,
        |v| !env_fvs.comps.contains(&v) && !u.is_weak_comp(v),
        |v| !env_fvs.tys.contains(&v) && !u.is_weak_ty(v),
    );

    // Cyclic roots already appear in `*_bindings`; drop them from the plain
    // quantifier sets so instantiation does not mint two fresh ids for one var.
    let cyclic_comp_roots: std::collections::HashSet<CompTyVar> =
        comp_ty_bindings.iter().map(|(r, _)| *r).collect();
    let comp_ty_vars: Vec<CompTyVar> = comp_ty_vars
        .filter(|v| !cyclic_comp_roots.contains(v))
        .collect();
    let cyclic_ty_roots: std::collections::HashSet<TyVar> =
        ty_bindings.iter().map(|(r, _)| *r).collect();
    let ty_vars: Vec<(TyVar, Kind)> = ty_vars
        .filter(|v| !cyclic_ty_roots.contains(v))
        .map(|v| (v, u.kinded(v).kind))
        .collect();

    let mut scheme = Scheme {
        ty_vars,
        comp_ty_vars,
        row_vars,
        grade_vars,
        ty: u.apply_ty_keeping_weak(ty),
        comp_ty_bindings,
        ty_bindings,
        cached_fv,
        weak,
    };
    // Kept weak anchors can already be bound; settlement still needs them.
    scheme.weak = scheme_free_vars(&scheme).weak(u);
    scheme
}

/// The computation and value cycles of a type, each a `(root, binding)`.
type CycleBindings = (Vec<(CompTyVar, CompTy)>, Vec<(TyVar, Ty)>);

/// The cycles reachable from `ty`, as `(root, binding)` snapshots of those
/// roots `keep_comp` and `keep_ty` admit.
fn cyclic_bindings(
    u: &Unifier,
    ty: &Ty,
    keep_comp: impl Fn(CompTyVar) -> bool,
    keep_ty: impl Fn(TyVar) -> bool,
) -> CycleBindings {
    let (comp_roots, ty_roots) = u.cyclic_roots_in_ty(ty);
    let comps = comp_roots
        .into_iter()
        .filter(|&r| keep_comp(r))
        .map(|root| {
            let binding = u
                .resolved_comp_root_binding(root)
                .unwrap_or(CompTy::Var(root));
            (root, binding)
        })
        .collect();
    let tys = ty_roots
        .into_iter()
        .filter(|&r| keep_ty(r))
        .map(|root| {
            let binding = u.resolved_ty_root_binding(root).unwrap_or(Ty::Var(root));
            (root, binding)
        })
        .collect();
    (comps, tys)
}

/// A scheme is *closed* when every variable in its body and equations is quantified,
/// captured as a cyclic-binding root, or a marked weak residual.  Any other
/// unquantified residual is an id no live slot owns, which `Store` in
/// `unify.rs` tolerates as free and a later `fresh()` can re-mint, aliasing
/// two unrelated variables.
pub(crate) fn scheme_is_closed(scheme: &Scheme) -> bool {
    // Stored binders are names, not slots in the caller's unifier.
    let fvs = scheme_free_vars(scheme);
    let ty_roots: std::collections::HashSet<TyVar> =
        scheme.ty_bindings.iter().map(|(r, _)| *r).collect();
    let comp_roots: std::collections::HashSet<CompTyVar> =
        scheme.comp_ty_bindings.iter().map(|(r, _)| *r).collect();
    fvs.tys.iter().all(|v| {
        scheme.ty_vars.iter().any(|(q, _)| q == v)
            || ty_roots.contains(v)
            || scheme.weak.tys.contains_key(v)
    }) && fvs.comps.iter().all(|v| {
        scheme.comp_ty_vars.contains(v) || comp_roots.contains(v) || scheme.weak.comps.contains(v)
    }) && fvs
        .rows
        .iter()
        .all(|v| scheme.row_vars.iter().any(|(q, _)| q == v) || scheme.weak.rows.contains_key(v))
        && fvs.grades.iter().all(|v| scheme.grade_vars.contains(v))
}

/// Syntactic variables in the entire scheme graph, including μ-equations.
fn scheme_free_vars(scheme: &Scheme) -> FreeVars {
    let u = Unifier::new();
    let mut fvs = FreeVars::new();
    free_ty(&u, &scheme.ty, &mut fvs);
    for (_, binding) in &scheme.ty_bindings {
        free_ty(&u, binding, &mut fvs);
    }
    for (_, binding) in &scheme.comp_ty_bindings {
        free_ty(&u, &Ty::Thunk(Box::new(binding.clone())), &mut fvs);
    }
    fvs
}

/// Whether `scheme` quantifies a variable that its curried body's result
/// mentions and none of its arguments does: a result nothing the caller
/// supplies determines, which is a cast.  A scheme that is not a block has no
/// arguments, and is no cast.
#[cfg(any(test, feature = "test-util"))]
pub(crate) fn has_result_only_var(scheme: &Scheme) -> bool {
    let u = Unifier::new();
    let Ty::Thunk(body) = &scheme.ty else {
        return false;
    };
    let (mut args, mut cur) = (FreeVars::new(), &**body);
    let result = loop {
        match cur {
            CompTy::Fun(arg, rest) => {
                free_ty(&u, arg, &mut args);
                cur = rest;
            }
            CompTy::Return(_, result) => break result,
            CompTy::Var(_) => return false,
        }
    };
    let mut results = FreeVars::new();
    free_ty(&u, result, &mut results);
    results.tys.iter().any(|v| {
        !args.tys.contains(v) && scheme.ty_vars.iter().any(|(quantified, _)| quantified == v)
    }) || results.rows.iter().any(|v| {
        !args.rows.contains(v)
            && scheme
                .row_vars
                .iter()
                .any(|(quantified, _)| quantified == v)
    })
}

/// Guards closure at the three empty-environment generalisation sites —
/// `annotate`'s `Bind` rule, `alias_arm_scheme`, `binding_value_scheme` — so a
/// violation trips there, not in a later run.  `msg` names the site.
pub(crate) fn debug_assert_scheme_closed(scheme: &Scheme, msg: &str) {
    debug_assert!(scheme_is_closed(scheme), "{msg}");
}

/// `scheme` as its unit leaves it: each weak variable it mentions resolved as
/// far as the unit fixed it, a cycle through one snapshotted like any other,
/// and the still-free ones recorded.
pub(crate) fn settle_weak(u: &Unifier, scheme: Arc<Scheme>) -> Arc<Scheme> {
    if scheme.weak.is_empty() {
        return scheme;
    }
    // Only residuals belong to `u`; local ∀/μ names stay opaque.
    let sm = SubstMap {
        tm: scheme
            .weak
            .tys
            .keys()
            .map(|&v| (v, u.apply_ty(&Ty::Var(v))))
            .collect(),
        rm: scheme
            .weak
            .rows
            .keys()
            .map(|&v| (v, u.apply_row(&Row::Var(v))))
            .collect(),
        cm: scheme
            .weak
            .comps
            .iter()
            .map(|&v| (v, u.apply_comp_ty(&CompTy::Var(v))))
            .collect(),
        gm: HashMap::new(),
    };
    let mut settled = (*scheme).clone();
    sm.graph(&mut settled);
    let mut comps: std::collections::BTreeMap<_, _> =
        settled.comp_ty_bindings.iter().cloned().collect();
    let mut tys: std::collections::BTreeMap<_, _> = settled.ty_bindings.iter().cloned().collect();
    // Only substitution images are live graphs; scheme-local binders are opaque.
    let bodies = sm
        .tm
        .values()
        .cloned()
        .chain(sm.rm.values().cloned().map(Ty::Record))
        .chain(sm.cm.values().cloned().map(|b| Ty::Thunk(Box::new(b))));
    for body in bodies {
        let (new_comps, new_tys) = cyclic_bindings(
            u,
            &body,
            |v| !comps.contains_key(&v),
            |v| !tys.contains_key(&v),
        );
        comps.extend(new_comps);
        tys.extend(new_tys);
    }
    settled.comp_ty_bindings = comps.into_iter().collect();
    settled.ty_bindings = tys.into_iter().collect();
    let mut fvs = scheme_free_vars(&settled);
    fvs.remove_quantified(&settled);
    fvs.tys
        .retain(|v| !settled.ty_bindings.iter().any(|(r, _)| r == v));
    fvs.comps
        .retain(|v| !settled.comp_ty_bindings.iter().any(|(r, _)| r == v));
    settled.weak = fvs.weak(u);
    Arc::new(settled)
}

/// A stored scheme in a later unit: its weak residuals are ids of a unifier
/// that is gone, so each becomes a fresh weak variable here.
pub(crate) fn reseed_weak(u: &mut Unifier, scheme: Arc<Scheme>) -> Arc<Scheme> {
    if scheme.weak.is_empty() {
        return scheme;
    }
    let tm: HashMap<TyVar, TyVar> = scheme
        .weak
        .tys
        .iter()
        .map(|(&v, &kind)| (v, u.fresh_kinded_var(kind)))
        .collect();
    let rm: HashMap<RowVar, RowVar> = scheme
        .weak
        .rows
        .iter()
        .map(|(&v, &deep)| (v, u.fresh_deep_row_var(deep)))
        .collect();
    let cm: HashMap<CompTyVar, CompTyVar> = scheme
        .weak
        .comps
        .iter()
        .map(|&v| (v, u.fresh_comp_root()))
        .collect();
    let weak = WeakVars {
        tys: scheme
            .weak
            .tys
            .iter()
            .map(|(v, &kind)| (tm[v], kind))
            .collect(),
        comps: cm.values().copied().collect(),
        rows: scheme
            .weak
            .rows
            .iter()
            .map(|(v, &deep)| (rm[v], deep))
            .collect(),
    };
    u.mark_weak_vars(&weak);
    let mut sm = fresh_binders(u, &scheme);
    sm.tm.extend(tm.into_iter().map(|(v, f)| (v, Ty::Var(f))));
    sm.rm.extend(rm.into_iter().map(|(v, f)| (v, Row::Var(f))));
    sm.cm
        .extend(cm.into_iter().map(|(v, f)| (v, CompTy::Var(f))));
    let mut seeded = (*scheme).clone();
    sm.graph(&mut seeded);
    for (v, _) in &mut seeded.ty_vars {
        *v = sm.ty_var(*v);
    }
    for v in &mut seeded.comp_ty_vars {
        *v = sm.comp_var(*v);
    }
    for (v, _) in &mut seeded.row_vars {
        let Row::Var(fresh) = sm.rm[v] else {
            unreachable!()
        };
        *v = fresh;
    }
    for v in &mut seeded.grade_vars {
        let Grade::Var(fresh) = sm.gm[v] else {
            unreachable!()
        };
        *v = fresh;
    }
    for (v, _) in &mut seeded.ty_bindings {
        *v = sm.ty_var(*v);
    }
    for (v, _) in &mut seeded.comp_ty_bindings {
        *v = sm.comp_var(*v);
    }
    seeded.weak = weak;
    Arc::new(seeded)
}

pub(crate) fn instantiate(u: &mut Unifier, scheme: &Scheme) -> Ty {
    if !scheme.is_poly() {
        return scheme.ty.clone();
    }
    let sm = fresh_binders(u, scheme);
    // Re-binding the fresh roots is what carries the cycle across instantiation.
    for (old, binding) in &scheme.comp_ty_bindings {
        let fresh_root = sm.comp_var(*old);
        let substituted = sm.comp(binding);
        u.bind_comp_root(fresh_root, substituted);
    }
    for (old, binding) in &scheme.ty_bindings {
        let fresh_root = sm.ty_var(*old);
        let substituted = sm.ty(binding);
        u.bind_ty_root(fresh_root, substituted);
    }
    sm.ty(&scheme.ty)
}

/// Reserve distinct live names for every local binder before substitution.
fn fresh_binders(u: &mut Unifier, scheme: &Scheme) -> SubstMap {
    let mut tm: HashMap<_, _> = scheme
        .ty_vars
        .iter()
        .map(|&(v, k)| (v, Ty::Var(u.fresh_kinded_var(k))))
        .collect();
    tm.extend(
        scheme
            .ty_bindings
            .iter()
            .map(|(v, _)| (*v, Ty::Var(u.fresh_ty_root()))),
    );
    SubstMap {
        tm,
        cm: scheme
            .comp_ty_vars
            .iter()
            .copied()
            .chain(scheme.comp_ty_bindings.iter().map(|(v, _)| *v))
            .map(|v| (v, CompTy::Var(u.fresh_comp_root())))
            .collect(),
        rm: scheme
            .row_vars
            .iter()
            .map(|&(v, deep)| (v, Row::Var(u.fresh_deep_row_var(deep))))
            .collect(),
        gm: scheme
            .grade_vars
            .iter()
            .map(|&v| (v, Grade::Var(u.fresh_grade_var())))
            .collect(),
    }
}

/// Simultaneous, non-recursive substitution over all four variable kinds.
struct SubstMap {
    tm: HashMap<TyVar, Ty>,
    rm: HashMap<RowVar, Row>,
    cm: HashMap<CompTyVar, CompTy>,
    gm: HashMap<GradeVar, Grade>,
}

impl SubstMap {
    fn ty_var(&self, v: TyVar) -> TyVar {
        let Ty::Var(fresh) = self.tm[&v] else {
            unreachable!()
        };
        fresh
    }

    fn comp_var(&self, v: CompTyVar) -> CompTyVar {
        let CompTy::Var(fresh) = self.cm[&v] else {
            unreachable!()
        };
        fresh
    }

    fn graph(&self, scheme: &mut Scheme) {
        scheme.ty = self.ty(&scheme.ty);
        for (_, binding) in &mut scheme.ty_bindings {
            *binding = self.ty(binding);
        }
        for (_, binding) in &mut scheme.comp_ty_bindings {
            *binding = self.comp(binding);
        }
    }

    fn ty(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(v) => self.tm.get(v).unwrap_or(ty).clone(),
            Ty::List(a) => Ty::list(self.ty(a)),
            Ty::Map(a) => Ty::map(self.ty(a)),
            Ty::Handle(g, a) => Ty::Handle(self.grade(*g), Box::new(self.ty(a))),
            Ty::Record(r) => Ty::Record(self.row(r)),
            Ty::Variant(r) => Ty::Variant(self.row(r)),
            Ty::Thunk(b) => Ty::Thunk(Box::new(self.comp(b))),
            // Enumerated, not `_`, as in `free_ty_inner`: cloning a new
            // variable-carrying `Ty` unsubstituted would be variable capture.
            Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String => ty.clone(),
        }
    }

    fn row(&self, row: &Row) -> Row {
        match row {
            Row::Empty => Row::Empty,
            Row::Var(v) => self.rm.get(v).unwrap_or(row).clone(),
            Row::Extend(l, ty, rest) => {
                Row::Extend(l.clone(), Box::new(self.ty(ty)), Box::new(self.row(rest)))
            }
        }
    }

    fn comp(&self, cty: &CompTy) -> CompTy {
        match cty {
            CompTy::Var(v) => self.cm.get(v).unwrap_or(cty).clone(),
            CompTy::Return(grade, a) => CompTy::Return(self.grade(*grade), Box::new(self.ty(a))),
            CompTy::Fun(a, b) => CompTy::Fun(Box::new(self.ty(a)), Box::new(self.comp(b))),
        }
    }

    fn grade(&self, grade: Grade) -> Grade {
        match grade {
            Grade::Var(v) => self.gm.get(&v).copied().unwrap_or(grade),
            _ => grade,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::unify::WeakSource;
    use super::*;
    use crate::ty::Label;

    fn generalized(u: &Unifier, ty: &Ty) -> Scheme {
        generalize(u, &TyEnv::new(), &FreeVars::new(), ty)
    }

    fn arrow(param: Ty, result: Ty) -> Ty {
        Ty::Thunk(Box::new(CompTy::Fun(
            Box::new(param),
            Box::new(CompTy::pure(result)),
        )))
    }

    #[test]
    fn cached_residuals_follow_later_structure() {
        for kind in 0..3 {
            let mut u = Unifier::new();
            let inner = u.fresh_tyvar();
            let (seed, structure) = match kind {
                0 => (Ty::Var(u.fresh_tyvar()), Ty::list(Ty::Var(inner))),
                1 => (
                    Ty::Thunk(Box::new(u.fresh_comp_ty())),
                    Ty::Thunk(Box::new(CompTy::pure(Ty::Var(inner)))),
                ),
                _ => (
                    Ty::Record(Row::Var(u.fresh_row_var())),
                    Ty::Record(Row::Extend(
                        Label::Field("a".into()),
                        Box::new(Ty::Var(inner)),
                        Box::new(Row::Empty),
                    )),
                ),
            };
            let mut enclosing = TyEnv::new();
            enclosing.bind("param".into(), Scheme::mono(seed.clone()));
            let local = generalize(&u, &enclosing, &FreeVars::new(), &seed);
            let mut env = TyEnv::new();
            env.bind("local".into(), local);
            u.unify_ty(&seed, &structure).unwrap();
            assert!(env_free_vars(&u, &env).tys.contains(&inner));
            let next = generalize(&u, &env, &FreeVars::new(), &Ty::Var(inner));
            assert!(next.ty_vars.is_empty(), "residual kind {kind}");
        }
    }

    #[test]
    fn a_weak_variable_survives_generalisation() {
        let mut u = Unifier::new();
        let (bound, weak) = (u.fresh_tyvar(), u.fresh_tyvar());
        u.mark_weak(&Ty::Var(weak), &WeakSource::Index);
        let scheme = generalized(&u, &arrow(Ty::Var(bound), Ty::Var(weak)));
        assert_eq!(scheme.ty_vars, vec![(bound, Kind::ANY)]);
        assert_eq!(scheme.weak.tys, [(weak, Kind::ANY)].into());
    }

    #[test]
    fn weak_rows_and_computations_survive_generalisation() {
        let mut u = Unifier::new();
        let (row, comp) = (u.fresh_row_var(), u.fresh_comp_ty());
        let ty = Ty::list(Ty::Record(Row::Var(row)));
        let body = Ty::Thunk(Box::new(comp.clone()));
        u.mark_weak(&ty, &WeakSource::Index);
        u.mark_weak(&body, &WeakSource::Index);
        let CompTy::Var(comp) = comp else {
            unreachable!("fresh_comp_ty yields a Var")
        };
        let scheme = generalized(&u, &arrow(ty, body));
        assert!(scheme.row_vars.is_empty() && scheme.comp_ty_vars.is_empty());
        assert_eq!(
            (scheme.weak.rows, scheme.weak.comps),
            ([(row, false)].into(), [comp].into())
        );
    }

    #[test]
    fn a_variable_united_with_a_weak_one_is_weak_whichever_side_it_is_on() {
        for weak_first in [true, false] {
            let mut u = Unifier::new();
            let (weak, other) = (u.fresh_tyvar(), u.fresh_tyvar());
            u.mark_weak(&Ty::Var(weak), &WeakSource::Index);
            let (a, b) = (Ty::Var(weak), Ty::Var(other));
            let result = if weak_first {
                u.unify_ty(&a, &b)
            } else {
                u.unify_ty(&b, &a)
            };
            result.expect("two variables unite");
            assert!(u.is_weak_ty(other) && u.is_weak_ty(weak));
            let scheme = generalized(&u, &arrow(b, Ty::Unit));
            assert_eq!(scheme.ty_vars, Vec::<(TyVar, Kind)>::new());
        }
    }

    #[test]
    fn binding_a_weak_variable_to_a_structure_makes_what_it_mentions_weak() {
        let mut u = Unifier::new();
        let (weak, inner) = (u.fresh_tyvar(), u.fresh_tyvar());
        u.mark_weak(&Ty::Var(weak), &WeakSource::Index);
        u.unify_ty(&Ty::Var(weak), &Ty::list(Ty::Var(inner)))
            .expect("a free variable binds");
        let scheme = generalized(&u, &arrow(Ty::Var(weak), Ty::Unit));
        assert_eq!(scheme.ty_vars, Vec::<(TyVar, Kind)>::new());
        assert!(u.is_weak_ty(inner));
        assert!(scheme.weak.tys.contains_key(&weak));
        let settled = settle_weak(&u, Arc::new(scheme));
        assert!(settled.weak.tys.contains_key(&inner));
    }

    /// Weak rows and computations inherit too, and stay themselves in a scheme
    /// after they are fixed.
    #[test]
    fn a_row_unified_with_a_fixed_weak_row_is_weak_and_kept() {
        let mut u = Unifier::new();
        let (weak, other) = (u.fresh_row_var(), u.fresh_row_var());
        u.mark_weak(&Ty::Record(Row::Var(weak)), &WeakSource::Index);
        let spine = Row::Extend(
            Label::Field("a".into()),
            Box::new(Ty::Int),
            Box::new(Row::Empty),
        );
        u.unify_row(&Row::Var(weak), &spine)
            .expect("a free row binds");
        u.unify_row(&Row::Var(other), &Row::Var(weak))
            .expect("a free row binds the spine");
        assert!(u.is_weak_row(other));
        let kept = u.apply_ty_keeping_weak(&Ty::Record(Row::Var(weak)));
        assert_eq!(kept, Ty::Record(Row::Var(weak)));
        assert_ne!(u.apply_ty(&Ty::Record(Row::Var(weak))), kept);
    }

    #[test]
    fn a_computation_unified_with_a_fixed_weak_one_is_weak() {
        let mut u = Unifier::new();
        let (weak, other) = (u.fresh_comp_ty(), u.fresh_comp_ty());
        u.mark_weak(&Ty::Thunk(Box::new(weak.clone())), &WeakSource::Index);
        u.unify_comp_ty(&weak, &CompTy::pure(Ty::Int))
            .expect("a free computation binds");
        u.unify_comp_ty(&other, &weak)
            .expect("a free computation binds");
        let CompTy::Var(other) = other else {
            unreachable!("fresh_comp_ty yields a Var")
        };
        assert!(u.is_weak_comp(other));
    }

    /// A stored scheme carries ids of a unifier that is gone.
    #[test]
    fn a_residual_round_trips_and_reseeds_fresh_and_weak() {
        let mut first = Unifier::new();
        for _ in 0..1000 {
            first.fresh_tyvar();
        }
        let (bound, weak) = (first.fresh_tyvar(), first.fresh_tyvar());
        first.mark_weak(&Ty::Var(weak), &WeakSource::Index);
        let scheme = generalized(&first, &arrow(Ty::Var(bound), Ty::Var(weak)));
        let bytes = postcard::to_allocvec(&scheme).expect("a scheme serialises");
        let stored: Scheme = postcard::from_bytes(&bytes).expect("and reads back");
        assert_eq!(stored, scheme);

        let mut next = Unifier::new();
        let seeded = reseed_weak(&mut next, Arc::new(stored));
        let [fresh] = seeded.weak.tys.keys().copied().collect::<Vec<_>>()[..] else {
            panic!("one residual in, one residual out: {:?}", seeded.weak);
        };
        assert_ne!(fresh, weak);
        assert!(next.is_weak_ty(fresh));
        let [(bound, Kind::ANY)] = seeded.ty_vars[..] else {
            panic!("one quantified variable")
        };
        assert_ne!(bound, fresh);
        assert_eq!(seeded.ty, arrow(Ty::Var(bound), Ty::Var(fresh)));
    }

    #[test]
    fn a_quantified_variable_keeps_its_kind_across_instantiation() {
        let mut u = Unifier::new();
        let v = u.fresh_kinded_var(Kind::NUMBER);
        let scheme = generalized(&u, &arrow(Ty::Var(v), Ty::Var(v)));
        assert_eq!(scheme.ty_vars, vec![(v, Kind::NUMBER)]);
        assert_eq!(scheme.to_string(), "∀α:number. α → Returns α");
        let Ty::Thunk(body) = instantiate(&mut u, &scheme) else {
            panic!("a function scheme instantiates to a thunk")
        };
        let CompTy::Fun(param, _) = *body else {
            panic!("a function")
        };
        let Ty::Var(fresh) = *param else {
            panic!("a fresh variable")
        };
        assert_ne!(fresh, v);
        assert_eq!(u.kinded(fresh).kind, Kind::NUMBER);
    }

    #[test]
    fn a_deep_row_is_quantified_deep_and_instantiates_deep() {
        let mut u = Unifier::new();
        let row = u.fresh_deep_row_var(true);
        let scheme = generalized(&u, &arrow(Ty::Record(Row::Var(row)), Ty::Unit));
        assert_eq!(scheme.row_vars, vec![(row, true)]);
        let Ty::Thunk(body) = instantiate(&mut u, &scheme) else {
            panic!("a thunk")
        };
        let CompTy::Fun(param, _) = *body else {
            panic!("a function")
        };
        let Ty::Record(Row::Var(fresh)) = *param else {
            panic!("a record on a fresh row")
        };
        assert!(u.is_deep_row(fresh));
    }

    #[test]
    fn a_kind_round_trips_through_postcard_in_a_scheme_and_in_its_residuals() {
        let mut first = Unifier::new();
        let (bound, weak) = (
            first.fresh_kinded_var(Kind::SIZED),
            first.fresh_kinded_var(Kind::COMPARABLE),
        );
        first.mark_weak(&Ty::Var(weak), &WeakSource::Index);
        let scheme = generalized(&first, &arrow(Ty::Var(bound), Ty::Var(weak)));
        let bytes = postcard::to_allocvec(&scheme).expect("a scheme serialises");
        let stored: Scheme = postcard::from_bytes(&bytes).expect("and reads back");
        assert_eq!(stored, scheme);
        assert_eq!(stored.ty_vars, vec![(bound, Kind::SIZED)]);

        let mut next = Unifier::new();
        let seeded = reseed_weak(&mut next, Arc::new(stored));
        let [(fresh, kind)] = seeded
            .weak
            .tys
            .iter()
            .map(|(&v, &k)| (v, k))
            .collect::<Vec<_>>()[..]
        else {
            panic!("one residual in, one out: {:?}", seeded.weak);
        };
        assert_eq!(kind, Kind::COMPARABLE);
        assert_eq!(next.kinded(fresh).kind, Kind::COMPARABLE);
        assert!(next.is_weak_ty(fresh));
    }

    #[test]
    fn reseeding_cannot_capture_a_quantifier_or_a_recursive_binder() {
        for recursive in [false, true] {
            let mut first = Unifier::new();
            let (bound, weak) = (first.fresh_tyvar(), first.fresh_tyvar());
            first.mark_weak(&Ty::Var(weak), &WeakSource::Index);
            if recursive {
                first.bind_ty_root(bound, Ty::list(Ty::Var(bound)));
            }
            let scheme = generalized(&first, &arrow(Ty::Var(bound), Ty::Var(weak)));
            let mut next = Unifier::new();
            let seeded = reseed_weak(&mut next, Arc::new(scheme));
            let fresh_weak = *seeded.weak.tys.keys().next().unwrap();
            assert_eq!(fresh_weak, bound, "force the old namespace collision");
            let instance = instantiate(&mut next, &seeded);
            let Ty::Thunk(body) = instance else {
                panic!("a thunk")
            };
            let CompTy::Fun(param, rest) = *body else {
                panic!("a function")
            };
            let CompTy::Return(_, result) = *rest else {
                panic!("a result")
            };
            assert_eq!(*result, Ty::Var(fresh_weak));
            assert_ne!(*param, *result);
            next.unify_ty(&result, &Ty::Int)
                .expect("the weak result fixes independently");
            assert_ne!(next.apply_ty(&param), Ty::Int);
        }
    }

    #[test]
    fn reseeding_cannot_capture_row_or_computation_quantifiers() {
        let mut first = Unifier::new();
        let (bound_row, weak_row) = (first.fresh_row_var(), first.fresh_row_var());
        let (bound_comp, weak_comp) = (first.fresh_comp_ty(), first.fresh_comp_ty());
        let ty = arrow(
            arrow(
                Ty::Record(Row::Var(bound_row)),
                Ty::Thunk(Box::new(bound_comp)),
            ),
            arrow(
                Ty::Record(Row::Var(weak_row)),
                Ty::Thunk(Box::new(weak_comp)),
            ),
        );
        let Ty::Thunk(body) = &ty else { unreachable!() };
        let CompTy::Fun(_, rest) = &**body else {
            unreachable!()
        };
        let CompTy::Return(_, result) = &**rest else {
            unreachable!()
        };
        first.mark_weak(result, &WeakSource::Index);
        let scheme = generalized(&first, &ty);
        let mut next = Unifier::new();
        let seeded = reseed_weak(&mut next, Arc::new(scheme));
        assert!(!seeded.weak.rows.contains_key(&seeded.row_vars[0].0));
        assert!(!seeded.weak.comps.contains(&seeded.comp_ty_vars[0]));
        let instance = instantiate(&mut next, &seeded);
        let mut fvs = FreeVars::new();
        free_ty(&next, &instance, &mut fvs);
        assert_eq!(fvs.rows.len(), 2);
        assert_eq!(fvs.comps.len(), 2);
        assert!(scheme_is_closed(&seeded));
    }

    #[test]
    fn settling_refreshes_weak_references_inside_recursive_snapshots() {
        let mut u = Unifier::new();
        let (root, weak) = (u.fresh_tyvar(), u.fresh_tyvar());
        u.mark_weak(&Ty::Var(weak), &WeakSource::Index);
        u.bind_ty_root(root, Ty::list(arrow(Ty::Var(weak), Ty::Var(root))));
        let scheme = Arc::new(generalized(&u, &Ty::Var(root)));
        u.unify_ty(&Ty::Var(weak), &Ty::Int).unwrap();
        let settled = settle_weak(&u, scheme);
        assert!(settled.weak.is_empty());
        assert!(!scheme_free_vars(&settled).tys.contains(&weak));
        assert!(scheme_is_closed(&settled));
        let bytes = postcard::to_allocvec(&*settled).unwrap();
        let stored: Scheme = postcard::from_bytes(&bytes).unwrap();
        let mut next = Unifier::new();
        let instance = instantiate(&mut next, &stored);
        assert!(next.weak_source_in_ty(&instance).is_none());
        assert_eq!(next.cyclic_roots_in_ty(&instance).1.len(), 1);
    }

    #[test]
    fn settling_an_imported_computation_cycle_walks_its_equation() {
        let mut first = Unifier::new();
        let weak = first.fresh_tyvar();
        let root = first.fresh_comp_root();
        first.mark_weak(&Ty::Var(weak), &WeakSource::Index);
        let body = CompTy::pure(Ty::list(arrow(
            Ty::Var(weak),
            Ty::Thunk(Box::new(CompTy::Var(root))),
        )));
        let mut scheme = Scheme::mono(Ty::Thunk(Box::new(CompTy::Var(root))));
        scheme.comp_ty_bindings.push((root, body));
        scheme.weak.tys.insert(weak, Kind::ANY);
        let mut next = Unifier::new();
        let seeded = reseed_weak(&mut next, Arc::new(scheme));
        let weak = *seeded.weak.tys.keys().next().unwrap();
        next.unify_ty(&Ty::Var(weak), &Ty::list(Ty::Var(weak)))
            .unwrap();
        assert!(
            next.weak_source_in_ty(&seeded.ty).is_none(),
            "the local μ-name is opaque"
        );
        let settled = settle_weak(&next, seeded);
        assert!(settled.weak.is_empty());
        assert_eq!(
            settled.ty_bindings.len(),
            1,
            "capture the new weak cycle inside the equation"
        );
        assert!(scheme_is_closed(&settled));
        let mut later = Unifier::new();
        let instance = instantiate(&mut later, &settled);
        let (comps, tys) = later.cyclic_roots_in_ty(&instance);
        assert_eq!((comps.len(), tys.len()), (1, 1));
    }

    #[test]
    fn closure_checks_snapshot_bodies_without_following_live_slots() {
        let mut u = Unifier::new();
        let root = u.fresh_tyvar();
        let dangling = u.fresh_tyvar();
        u.unify_ty(&Ty::Var(dangling), &Ty::Int).unwrap();
        let mut scheme = Scheme::mono(Ty::Var(root));
        scheme.ty_bindings.push((root, Ty::list(Ty::Var(dangling))));
        assert!(!scheme_is_closed(&scheme));
        scheme.ty_bindings[0].1 = Ty::list(Ty::Var(root));
        assert!(scheme_is_closed(&scheme));
        scheme
            .comp_ty_bindings
            .push((CompTyVar(0), CompTy::pure(Ty::Record(Row::Var(RowVar(0))))));
        assert!(!scheme_is_closed(&scheme));
    }

    #[test]
    fn settlement_does_not_resolve_local_binders_in_the_live_unifier() {
        let mut u = Unifier::new();
        let (local, weak) = (u.fresh_tyvar(), u.fresh_tyvar());
        u.mark_weak(&Ty::Var(weak), &WeakSource::Index);
        let scheme = Arc::new(generalized(&u, &arrow(Ty::Var(local), Ty::Var(weak))));
        u.bind_ty_root(local, Ty::list(Ty::Var(local)));
        u.unify_ty(&Ty::Var(weak), &Ty::Int).unwrap();
        let settled = settle_weak(&u, scheme);
        assert_eq!(settled.ty, arrow(Ty::Var(local), Ty::Int));
        assert_eq!(settled.ty_bindings, []);
        assert!(settled.weak.is_empty());
    }

    #[test]
    fn already_fixed_weak_anchors_are_still_settled() {
        let mut u = Unifier::new();
        let weak = u.fresh_tyvar();
        u.mark_weak(&Ty::Var(weak), &WeakSource::Index);
        u.unify_ty(&Ty::Var(weak), &Ty::Int).unwrap();
        let scheme = Arc::new(generalized(&u, &Ty::Var(weak)));
        let settled = settle_weak(&u, scheme);
        assert_eq!(settled.ty, Ty::Int);
        assert!(settled.weak.is_empty());
        assert!(scheme_is_closed(&settled));
    }

    #[test]
    fn a_unit_settles_what_it_fixed_before_storing() {
        let mut u = Unifier::new();
        let (fixed, open) = (u.fresh_tyvar(), u.fresh_tyvar());
        let ty = arrow(Ty::Var(fixed), Ty::Var(open));
        u.mark_weak(&ty, &WeakSource::Index);
        let scheme = Arc::new(generalized(&u, &ty));
        u.unify_ty(&Ty::Var(fixed), &Ty::Int)
            .expect("a free variable binds");
        let settled = settle_weak(&u, scheme);
        assert_eq!(settled.ty, arrow(Ty::Int, Ty::Var(open)));
        assert_eq!(settled.weak.tys, [(open, Kind::ANY)].into());
    }
}
