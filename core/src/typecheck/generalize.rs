//! Generalization and instantiation for HM polymorphism.
//!
//! Equi-recursion is the twist: a cyclic type is anchored at a union-find
//! root, so a scheme snapshots that root's binding and `instantiate`
//! re-anchors it at a fresh root instead of sharing the original slot.

use super::env::TyEnv;
use super::kind::Kind;
use super::scheme::{Scheme, WeakVars};
use super::ty::{CompTy, CompTyVar, Row, RowVar, Ty, TyVar};
use super::unify::{Unifier, Visited};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// All three variable kinds, collected in one traversal.
#[derive(Clone)]
pub(crate) struct FreeVars {
    pub(crate) tys: HashSet<TyVar>,
    pub(crate) comps: HashSet<CompTyVar>,
    pub(crate) rows: HashSet<RowVar>,
}

impl FreeVars {
    pub fn new() -> Self {
        Self {
            tys: HashSet::new(),
            comps: HashSet::new(),
            rows: HashSet::new(),
        }
    }

    pub(crate) fn merge_cached(&mut self, cached: &super::scheme::CachedFreeVars) {
        self.tys.extend(&cached.ty_fv);
        self.comps.extend(&cached.comp_fv);
        self.rows.extend(&cached.row_fv);
    }

    pub(crate) fn merge_into(self, target: &mut Self) {
        target.tys.extend(self.tys);
        target.comps.extend(self.comps);
        target.rows.extend(self.rows);
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
    }

    /// The *residual* free vars — mentioned by `env`, so left unquantified.
    pub(crate) fn intersect_into_cached(&self, env: &Self) -> super::scheme::CachedFreeVars {
        super::scheme::CachedFreeVars {
            ty_fv: self.tys.intersection(&env.tys).copied().collect(),
            comp_fv: self.comps.intersection(&env.comps).copied().collect(),
            row_fv: self.rows.intersection(&env.rows).copied().collect(),
        }
    }
}

pub(crate) fn free_ty(u: &Unifier, ty: &Ty, out: &mut FreeVars) {
    let mut visited = Visited::default();
    free_ty_inner(u, ty, out, &mut visited);
}

fn free_ty_inner(u: &Unifier, ty: &Ty, out: &mut FreeVars, visited: &mut Visited) {
    // Cycle guard.  Skipping a sibling revisit is sound because the walk never
    // binds: the first visit collected every free var behind this root.
    let root = match ty {
        Ty::Var(TyVar(i)) => Some(u.ty_root(*i)),
        _ => None,
    };
    if let Some(r) = root
        && !visited.tys.insert(r)
    {
        return;
    }
    match &*u.head_ty(ty) {
        Ty::Var(v) => {
            out.tys.insert(*v);
        }
        Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => free_ty_inner(u, a, out, visited),
        Ty::Record(r) | Ty::Variant(r) => free_row_inner(u, r, out, visited),
        Ty::Thunk(b) => free_comp_inner(u, b, out, visited),
        // Enumerated, not `_`: a new `Ty` carrying variables then fails the
        // build here instead of being dropped and silently under-generalised.
        Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String => {}
    }
}

fn free_row_inner(u: &Unifier, row: &Row, out: &mut FreeVars, visited: &mut Visited) {
    match &*u.head_row(row) {
        Row::Empty => {}
        Row::Var(v) => {
            out.rows.insert(*v);
        }
        Row::Extend(_, ty, rest) => {
            free_ty_inner(u, ty, out, visited);
            free_row_inner(u, rest, out, visited);
        }
    }
}

fn free_comp_inner(u: &Unifier, cty: &CompTy, out: &mut FreeVars, visited: &mut Visited) {
    // Cycle guard, same reasoning as `free_ty_inner`.
    let root = match cty {
        CompTy::Var(CompTyVar(i)) => Some(u.comp_root(*i)),
        _ => None,
    };
    if let Some(r) = root
        && !visited.comps.insert(r)
    {
        return;
    }
    match &*u.head_comp_ty(cty) {
        CompTy::Var(v) => {
            out.comps.insert(*v);
        }
        CompTy::Return(a) => free_ty_inner(u, a, out, visited),
        CompTy::Fun(a, b) => {
            free_ty_inner(u, a, out, visited);
            free_comp_inner(u, b, out, visited);
        }
    }
}

pub(crate) fn env_free_vars(u: &Unifier, env: &TyEnv) -> FreeVars {
    let mut out = FreeVars::new();
    for s in env.all_schemes() {
        if let Some(cached) = &s.cached_fv {
            out.merge_cached(cached);
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

    // Cached so later `env_free_vars` calls read the sets instead of re-walking
    // this scheme's type tree.  Empty for top-level bindings.
    let residuals = fvs.intersect_into_cached(&env_fvs);
    // Holds by construction: residuals come from monomorphic env bindings that
    // outlive every scheme mentioning them, so no later step moves or binds one.
    debug_assert!(
        residuals_are_live_roots(u, &residuals),
        "a generalisation residual is not an unbound canonical root — the cache \
         would go stale under a later unite/bind"
    );
    let cached_fv = Some(residuals);

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
    let cyclic_comp_roots: std::collections::HashSet<u32> =
        comp_ty_bindings.iter().map(|(r, _)| *r).collect();
    let comp_ty_vars: Vec<CompTyVar> = comp_ty_vars
        .filter(|v| !cyclic_comp_roots.contains(&v.0))
        .collect();
    let cyclic_ty_roots: std::collections::HashSet<u32> =
        ty_bindings.iter().map(|(r, _)| *r).collect();
    let ty_vars: Vec<(TyVar, Kind)> = ty_vars
        .filter(|v| !cyclic_ty_roots.contains(&v.0))
        .map(|v| (v, u.kinded(v).kind))
        .collect();

    Scheme {
        ty_vars,
        comp_ty_vars,
        row_vars,
        ty: u.apply_ty_keeping_weak(ty),
        comp_ty_bindings,
        ty_bindings,
        cached_fv,
        weak,
    }
}

/// The computation and value cycles of a type, each a `(root, binding)`.
type CycleBindings = (Vec<(u32, CompTy)>, Vec<(u32, Ty)>);

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
        .filter(|&r| keep_comp(CompTyVar(r)))
        .map(|root| {
            let binding = u
                .resolved_comp_root_binding(root)
                .unwrap_or(CompTy::Var(CompTyVar(root)));
            (root, binding)
        })
        .collect();
    let tys = ty_roots
        .into_iter()
        .filter(|&r| keep_ty(TyVar(r)))
        .map(|root| {
            let binding = u
                .resolved_ty_root_binding(root)
                .unwrap_or(Ty::Var(TyVar(root)));
            (root, binding)
        })
        .collect();
    (comps, tys)
}

fn residuals_are_live_roots(u: &Unifier, residuals: &super::scheme::CachedFreeVars) -> bool {
    residuals
        .ty_fv
        .iter()
        .all(|v| u.ty_root(v.0) == v.0 && matches!(u.resolve_ty(&Ty::Var(*v)), Ty::Var(_)))
        && residuals.comp_fv.iter().all(|v| {
            u.comp_root(v.0) == v.0 && matches!(u.resolve_comp_ty(&CompTy::Var(*v)), CompTy::Var(_))
        })
        && residuals
            .row_fv
            .iter()
            .all(|v| matches!(u.resolve_row(&Row::Var(*v)), Row::Var(rv) if rv == *v))
}

/// A scheme is *closed* when every free variable in its body is quantified,
/// captured as a cyclic-binding root, or a marked weak residual.  Any other
/// unquantified residual is an id no live slot owns, which `Store` in
/// `unify.rs` tolerates as free and a later `fresh()` can re-mint, aliasing
/// two unrelated variables.
pub(crate) fn scheme_is_closed(u: &Unifier, scheme: &Scheme) -> bool {
    let mut fvs = FreeVars::new();
    free_ty(u, &scheme.ty, &mut fvs);
    let ty_roots: std::collections::HashSet<u32> =
        scheme.ty_bindings.iter().map(|(r, _)| *r).collect();
    let comp_roots: std::collections::HashSet<u32> =
        scheme.comp_ty_bindings.iter().map(|(r, _)| *r).collect();
    fvs.tys.iter().all(|v| {
        scheme.ty_vars.iter().any(|(q, _)| q == v)
            || ty_roots.contains(&v.0)
            || scheme.weak.tys.contains_key(v)
    }) && fvs.comps.iter().all(|v| {
        scheme.comp_ty_vars.contains(v)
            || comp_roots.contains(&v.0)
            || scheme.weak.comps.contains(v)
    }) && fvs
        .rows
        .iter()
        .all(|v| scheme.row_vars.iter().any(|(q, _)| q == v) || scheme.weak.rows.contains_key(v))
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
            CompTy::Return(result) => break result,
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
pub(crate) fn debug_assert_scheme_closed(u: &Unifier, scheme: &Scheme, msg: &str) {
    debug_assert!(scheme_is_closed(u, scheme), "{msg}");
}

/// `scheme` as its unit leaves it: each weak variable it mentions resolved as
/// far as the unit fixed it, a cycle through one snapshotted like any other,
/// and the still-free ones recorded.
pub(crate) fn settle_weak(u: &Unifier, scheme: Arc<Scheme>) -> Arc<Scheme> {
    if !u.reaches_weak(&scheme.ty) {
        return scheme;
    }
    let ty = u.apply_ty(&scheme.ty);
    let (comps, tys) = cyclic_bindings(
        u,
        &ty,
        |v| !scheme.comp_ty_bindings.iter().any(|(r, _)| *r == v.0),
        |v| !scheme.ty_bindings.iter().any(|(r, _)| *r == v.0),
    );
    let mut fvs = FreeVars::new();
    free_ty(u, &ty, &mut fvs);
    fvs.tys.retain(|v| !tys.iter().any(|(r, _)| *r == v.0));
    fvs.comps.retain(|v| !comps.iter().any(|(r, _)| *r == v.0));
    Arc::new(Scheme {
        ty,
        weak: fvs.weak(u),
        comp_ty_bindings: [scheme.comp_ty_bindings.clone(), comps].concat(),
        ty_bindings: [scheme.ty_bindings.clone(), tys].concat(),
        ..(*scheme).clone()
    })
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
    let cm: HashMap<u32, u32> = scheme
        .weak
        .comps
        .iter()
        .map(|v| (v.0, u.fresh_comp_root()))
        .collect();
    let weak = WeakVars {
        tys: scheme
            .weak
            .tys
            .iter()
            .map(|(v, &kind)| (tm[v], kind))
            .collect(),
        comps: cm.values().map(|&id| CompTyVar(id)).collect(),
        rows: scheme
            .weak
            .rows
            .iter()
            .map(|(v, &deep)| (rm[v], deep))
            .collect(),
    };
    u.mark_weak_vars(&weak);
    let sm = SubstMap {
        tm,
        rm,
        cm,
        tcm: HashMap::new(),
    };
    Arc::new(Scheme {
        ty: sm.ty(&scheme.ty),
        comp_ty_bindings: scheme
            .comp_ty_bindings
            .iter()
            .map(|(root, binding)| (*root, sm.comp(binding)))
            .collect(),
        ty_bindings: scheme
            .ty_bindings
            .iter()
            .map(|(root, binding)| (*root, sm.ty(binding)))
            .collect(),
        weak,
        ..(*scheme).clone()
    })
}

pub(crate) fn instantiate(u: &mut Unifier, scheme: &Scheme) -> Ty {
    if !scheme.is_poly() {
        return scheme.ty.clone();
    }
    // A fresh union-find root per old id: two instantiations never share state.
    let mut cm: HashMap<u32, u32> = HashMap::new();
    for v in &scheme.comp_ty_vars {
        cm.insert(v.0, u.fresh_comp_root());
    }
    for (old, _) in &scheme.comp_ty_bindings {
        cm.insert(*old, u.fresh_comp_root());
    }
    let mut tcm: HashMap<u32, u32> = HashMap::new();
    for (old, _) in &scheme.ty_bindings {
        tcm.insert(*old, u.fresh_ty_root());
    }
    let sm = SubstMap {
        tm: scheme
            .ty_vars
            .iter()
            .map(|&(v, kind)| (v, u.fresh_kinded_var(kind)))
            .collect(),
        rm: scheme
            .row_vars
            .iter()
            .map(|&(v, deep)| (v, u.fresh_deep_row_var(deep)))
            .collect(),
        cm: cm.clone(),
        tcm: tcm.clone(),
    };
    // Re-binding the fresh roots is what carries the cycle across instantiation.
    for (old, binding) in &scheme.comp_ty_bindings {
        let fresh_root = cm[old];
        let substituted = sm.comp(binding);
        u.bind_comp_root(fresh_root, substituted);
    }
    for (old, binding) in &scheme.ty_bindings {
        let fresh_root = tcm[old];
        let substituted = sm.ty(binding);
        u.bind_ty_root(fresh_root, substituted);
    }
    sm.ty(&scheme.ty)
}

/// Simultaneous substitution over all three variable kinds.  `cm` and `tcm`
/// carry the cyclic back-edge roots — empty for non-recursive schemes.
struct SubstMap {
    tm: HashMap<TyVar, TyVar>,
    rm: HashMap<RowVar, RowVar>,
    cm: HashMap<u32, u32>,
    tcm: HashMap<u32, u32>,
}

impl SubstMap {
    fn ty(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(TyVar(i)) => {
                if let Some(&fresh) = self.tcm.get(i) {
                    return Ty::Var(TyVar(fresh));
                }
                self.tm
                    .get(&TyVar(*i))
                    .map_or_else(|| ty.clone(), |&f| Ty::Var(f))
            }
            Ty::List(a) => Ty::List(Box::new(self.ty(a))),
            Ty::Map(a) => Ty::Map(Box::new(self.ty(a))),
            Ty::Handle(a) => Ty::Handle(Box::new(self.ty(a))),
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
            Row::Var(v) => self.rm.get(v).map_or_else(|| row.clone(), |&f| Row::Var(f)),
            Row::Extend(l, ty, rest) => {
                Row::Extend(l.clone(), Box::new(self.ty(ty)), Box::new(self.row(rest)))
            }
        }
    }

    fn comp(&self, cty: &CompTy) -> CompTy {
        match cty {
            CompTy::Var(CompTyVar(i)) => {
                let id = *self.cm.get(i).unwrap_or(i);
                CompTy::Var(CompTyVar(id))
            }
            CompTy::Return(a) => CompTy::Return(Box::new(self.ty(a))),
            CompTy::Fun(a, b) => CompTy::Fun(Box::new(self.ty(a)), Box::new(self.comp(b))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ty::Label;
    use super::super::unify::WeakSource;
    use super::*;

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
        let ty = Ty::List(Box::new(Ty::Record(Row::Var(row))));
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
            assert!(scheme.ty_vars.is_empty());
        }
    }

    #[test]
    fn binding_a_weak_variable_to_a_structure_makes_what_it_mentions_weak() {
        let mut u = Unifier::new();
        let (weak, inner) = (u.fresh_tyvar(), u.fresh_tyvar());
        u.mark_weak(&Ty::Var(weak), &WeakSource::Index);
        u.unify_ty(&Ty::Var(weak), &Ty::List(Box::new(Ty::Var(inner))))
            .expect("a free variable binds");
        let scheme = generalized(&u, &arrow(Ty::Var(weak), Ty::Unit));
        assert!(scheme.ty_vars.is_empty());
        assert!(u.is_weak_ty(inner));
        assert!(scheme.weak.tys.contains_key(&inner));
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
        assert_eq!(seeded.ty_vars, vec![(bound, Kind::ANY)]);
        assert_eq!(seeded.ty, arrow(Ty::Var(bound), Ty::Var(fresh)));
    }

    #[test]
    fn a_quantified_variable_keeps_its_kind_across_instantiation() {
        let mut u = Unifier::new();
        let v = u.fresh_kinded_var(Kind::NUMBER);
        let scheme = generalized(&u, &arrow(Ty::Var(v), Ty::Var(v)));
        assert_eq!(scheme.ty_vars, vec![(v, Kind::NUMBER)]);
        assert_eq!(
            super::super::fmt::fmt_scheme(&scheme),
            "∀α:number. α → Command α"
        );
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
