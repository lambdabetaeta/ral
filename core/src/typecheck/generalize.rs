//! Generalization and instantiation for HM polymorphism.
//!
//! Equi-recursion is the twist: a cyclic type is anchored at a union-find
//! root, so a scheme snapshots that root's binding and `instantiate`
//! re-anchors it at a fresh root instead of sharing the original slot.

use super::env::TyEnv;
use super::scheme::Scheme;
use super::ty::{CompTy, CompTyVar, PayloadRoute, PayloadVar, Row, RowVar, Ty, TyVar};
use super::unify::{Unifier, Visited};
use std::collections::{HashMap, HashSet};

/// All four variable kinds, collected in one traversal.
pub struct FreeVars {
    pub tys: HashSet<TyVar>,
    pub comps: HashSet<CompTyVar>,
    pub routes: HashSet<PayloadVar>,
    pub rows: HashSet<RowVar>,
}

impl FreeVars {
    pub fn new() -> Self {
        Self {
            tys: HashSet::new(),
            comps: HashSet::new(),
            routes: HashSet::new(),
            rows: HashSet::new(),
        }
    }

    pub fn merge_cached(&mut self, cached: &super::scheme::CachedFreeVars) {
        self.tys.extend(&cached.ty_fv);
        self.comps.extend(&cached.comp_fv);
        self.routes.extend(&cached.route_fv);
        self.rows.extend(&cached.row_fv);
    }

    pub fn merge_into(self, target: &mut Self) {
        target.tys.extend(self.tys);
        target.comps.extend(self.comps);
        target.routes.extend(self.routes);
        target.rows.extend(self.rows);
    }

    /// Instantiation mints these fresh, so they are not free in the environment.
    pub fn remove_quantified(&mut self, s: &Scheme) {
        for v in &s.ty_vars {
            self.tys.remove(v);
        }
        for v in &s.comp_ty_vars {
            self.comps.remove(v);
        }
        for v in &s.route_vars {
            self.routes.remove(v);
        }
        for v in &s.row_vars {
            self.rows.remove(v);
        }
    }

    /// The *residual* free vars — mentioned by `env`, so left unquantified.
    pub fn intersect_into_cached(&self, env: &Self) -> super::scheme::CachedFreeVars {
        super::scheme::CachedFreeVars {
            ty_fv: self.tys.intersection(&env.tys).copied().collect(),
            comp_fv: self.comps.intersection(&env.comps).copied().collect(),
            route_fv: self.routes.intersection(&env.routes).copied().collect(),
            row_fv: self.rows.intersection(&env.rows).copied().collect(),
        }
    }
}

pub fn free_ty(u: &mut Unifier, ty: &Ty, out: &mut FreeVars) {
    let mut visited = Visited::default();
    free_ty_inner(u, ty, out, &mut visited);
}

fn free_ty_inner(u: &mut Unifier, ty: &Ty, out: &mut FreeVars, visited: &mut Visited) {
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
    match u.resolve_ty(ty) {
        Ty::Var(v) => {
            out.tys.insert(v);
        }
        Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => free_ty_inner(u, &a, out, visited),
        Ty::Record(r) | Ty::Variant(r) => free_row_inner(u, &r, out, visited),
        Ty::Thunk(b) => free_comp_inner(u, &b, out, visited),
        // Enumerated, not `_`: a new `Ty` carrying variables then fails the
        // build here instead of being dropped and silently under-generalised.
        Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String => {}
    }
}

fn free_row_inner(u: &mut Unifier, row: &Row, out: &mut FreeVars, visited: &mut Visited) {
    match u.resolve_row(row) {
        Row::Empty => {}
        Row::Var(v) => {
            out.rows.insert(v);
        }
        Row::Extend(_, ty, rest) => {
            free_ty_inner(u, &ty, out, visited);
            free_row_inner(u, &rest, out, visited);
        }
    }
}

fn free_comp_inner(u: &mut Unifier, cty: &CompTy, out: &mut FreeVars, visited: &mut Visited) {
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
    match u.resolve_comp_ty(cty) {
        CompTy::Var(v) => {
            out.comps.insert(v);
        }
        CompTy::Return(route, a) => {
            free_route_inner(u, route, out);
            free_ty_inner(u, &a, out, visited);
        }
        CompTy::Fun(a, b) => {
            free_ty_inner(u, &a, out, visited);
            free_comp_inner(u, &b, out, visited);
        }
    }
}

fn free_route_inner(u: &mut Unifier, route: PayloadRoute, out: &mut FreeVars) {
    if let PayloadRoute::Var(v) = u.resolve_route(&route) {
        out.routes.insert(v);
    }
}

pub fn env_free_vars(u: &mut Unifier, env: &TyEnv) -> FreeVars {
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

pub fn generalize(u: &mut Unifier, env: &TyEnv, ty: &Ty) -> Scheme {
    let applied = u.apply_ty(ty);

    let mut fvs = FreeVars::new();
    free_ty(u, &applied, &mut fvs);
    let env_fvs = env_free_vars(u, env);

    let ty_vars = fvs.tys.difference(&env_fvs.tys).copied();
    let comp_ty_vars = fvs.comps.difference(&env_fvs.comps).copied();
    let route_vars: Vec<PayloadVar> = fvs.routes.difference(&env_fvs.routes).copied().collect();
    let row_vars: Vec<RowVar> = fvs.rows.difference(&env_fvs.rows).copied().collect();

    // Cached so later `env_free_vars` calls read the sets instead of re-walking
    // this scheme's type tree.  Empty for top-level bindings.
    let residuals = fvs.intersect_into_cached(&env_fvs);
    // Holds by construction: residuals come from monomorphic env bindings that
    // outlive every scheme mentioning them, so no later step moves or binds one.
    #[allow(
        clippy::debug_assert_with_mut_call,
        reason = "the &mut is union-find path compression, semantically idempotent; skipping it in release is harmless"
    )]
    {
        debug_assert!(
            residuals_are_live_roots(u, &residuals),
            "a generalisation residual is not an unbound canonical root — the cache \
             would go stale under a later unite/bind"
        );
    }
    let cached_fv = Some(residuals);

    // `apply_*` leaves each cycle as a `Var(root)` back-edge; snapshotting root
    // and binding lets `instantiate` rebuild the cycle in a fresh slot.  Roots
    // free in the env are excluded, as from the quantifier sets above: they are
    // monomorphic and must stay anchored so one use's constraint reaches all.
    let env_comp_roots: std::collections::HashSet<u32> =
        env_fvs.comps.iter().map(|v| v.0).collect();
    let env_ty_roots: std::collections::HashSet<u32> = env_fvs.tys.iter().map(|v| v.0).collect();
    let comp_ty_bindings: Vec<(u32, CompTy)> = snapshot_cyclic_comp_bindings(u, &applied)
        .into_iter()
        .filter(|(r, _)| !env_comp_roots.contains(r))
        .collect();
    let ty_bindings: Vec<(u32, Ty)> = snapshot_cyclic_ty_bindings(u, &applied)
        .into_iter()
        .filter(|(r, _)| !env_ty_roots.contains(r))
        .collect();

    // Cyclic roots already appear in `*_bindings`; drop them from the plain
    // quantifier sets so instantiation does not mint two fresh ids for one var.
    let cyclic_comp_roots: std::collections::HashSet<u32> =
        comp_ty_bindings.iter().map(|(r, _)| *r).collect();
    let comp_ty_vars: Vec<CompTyVar> = comp_ty_vars
        .filter(|v| !cyclic_comp_roots.contains(&v.0))
        .collect();
    let cyclic_ty_roots: std::collections::HashSet<u32> =
        ty_bindings.iter().map(|(r, _)| *r).collect();
    let ty_vars: Vec<TyVar> = ty_vars
        .filter(|v| !cyclic_ty_roots.contains(&v.0))
        .collect();

    Scheme {
        ty_vars,
        comp_ty_vars,
        route_vars,
        row_vars,
        ty: applied,
        comp_ty_bindings,
        ty_bindings,
        cached_fv,
    }
}

fn residuals_are_live_roots(u: &mut Unifier, residuals: &super::scheme::CachedFreeVars) -> bool {
    residuals
        .ty_fv
        .iter()
        .all(|v| u.ty_root(v.0) == v.0 && matches!(u.resolve_ty(&Ty::Var(*v)), Ty::Var(_)))
        && residuals.comp_fv.iter().all(|v| {
            u.comp_root(v.0) == v.0 && matches!(u.resolve_comp_ty(&CompTy::Var(*v)), CompTy::Var(_))
        })
        && residuals
            .route_fv
            .iter()
            .all(|v| matches!(u.resolve_route(&PayloadRoute::Var(*v)), PayloadRoute::Var(rv) if rv == *v))
        && residuals
            .row_fv
            .iter()
            .all(|v| matches!(u.resolve_row(&Row::Var(*v)), Row::Var(rv) if rv == *v))
}

/// A scheme is *closed* when every free variable in its body is quantified or
/// captured as a cyclic-binding root.  An unquantified residual is an id no
/// live slot owns, which `Store` in `unify.rs` tolerates as free and a later
/// `fresh()` can re-mint, aliasing two unrelated variables.
pub fn scheme_is_closed(u: &mut Unifier, scheme: &Scheme) -> bool {
    let mut fvs = FreeVars::new();
    free_ty(u, &scheme.ty, &mut fvs);
    let ty_roots: std::collections::HashSet<u32> =
        scheme.ty_bindings.iter().map(|(r, _)| *r).collect();
    let comp_roots: std::collections::HashSet<u32> =
        scheme.comp_ty_bindings.iter().map(|(r, _)| *r).collect();
    fvs.tys
        .iter()
        .all(|v| scheme.ty_vars.contains(v) || ty_roots.contains(&v.0))
        && fvs
            .comps
            .iter()
            .all(|v| scheme.comp_ty_vars.contains(v) || comp_roots.contains(&v.0))
        && fvs.routes.iter().all(|v| scheme.route_vars.contains(v))
        && fvs.rows.iter().all(|v| scheme.row_vars.contains(v))
}

/// Guards closure at the three empty-environment generalisation sites —
/// `annotate`'s `Bind` rule, `alias_arm_scheme`, `binding_value_scheme` — so a
/// violation trips there, not in a later run.  `msg` names the site.
#[allow(
    clippy::debug_assert_with_mut_call,
    reason = "the &mut is union-find path compression, semantically idempotent; skipping it in release is harmless"
)]
pub fn debug_assert_scheme_closed(u: &mut Unifier, scheme: &Scheme, msg: &str) {
    debug_assert!(scheme_is_closed(u, scheme), "{msg}");
}

pub fn instantiate(u: &mut Unifier, scheme: &Scheme) -> Ty {
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
            .map(|&v| (v, u.fresh_tyvar()))
            .collect(),
        rtm: scheme
            .route_vars
            .iter()
            .map(|&v| (v, u.fresh_routevar()))
            .collect(),
        rm: scheme
            .row_vars
            .iter()
            .map(|&v| (v, u.fresh_row_var()))
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

/// Each cyclic comp-var root in `applied` with its binding.  The bindings are
/// themselves applied, cycle-aware, so re-binding fresh roots detaches the copy.
fn snapshot_cyclic_comp_bindings(u: &mut Unifier, applied: &Ty) -> Vec<(u32, CompTy)> {
    u.cyclic_comp_roots_in_ty(applied)
        .into_iter()
        .map(|root| {
            let binding = u
                .resolved_comp_root_binding(root)
                .unwrap_or(CompTy::Var(CompTyVar(root)));
            (root, binding)
        })
        .collect()
}

/// Mirror of `snapshot_cyclic_comp_bindings` for value-type cycles — the
/// streaming consumer `α := Variant {more {head, tail: Thunk(α)}, done | ρ}`.
fn snapshot_cyclic_ty_bindings(u: &mut Unifier, applied: &Ty) -> Vec<(u32, Ty)> {
    u.cyclic_ty_roots_in_ty(applied)
        .into_iter()
        .map(|root| {
            let binding = u
                .resolved_ty_root_binding(root)
                .unwrap_or(Ty::Var(TyVar(root)));
            (root, binding)
        })
        .collect()
}

/// Simultaneous substitution over all four variable kinds.  `cm` and `tcm`
/// carry the cyclic back-edge roots — empty for non-recursive schemes.
struct SubstMap {
    tm: HashMap<TyVar, TyVar>,
    rtm: HashMap<PayloadVar, PayloadVar>,
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
            CompTy::Return(route, a) => CompTy::Return(self.route(*route), Box::new(self.ty(a))),
            CompTy::Fun(a, b) => CompTy::Fun(Box::new(self.ty(a)), Box::new(self.comp(b))),
        }
    }

    fn route(&self, route: PayloadRoute) -> PayloadRoute {
        match route {
            PayloadRoute::Value | PayloadRoute::Bytes => route,
            PayloadRoute::Var(v) => self.rtm.get(&v).map_or(route, |&f| PayloadRoute::Var(f)),
        }
    }
}
