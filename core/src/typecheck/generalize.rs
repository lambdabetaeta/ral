//! Generalization and instantiation for HM polymorphism.
//!
//! `generalize` closes over the free type/mode/row variables in a type that
//! are not mentioned in the ambient environment, producing a ∀-quantified
//! scheme.  `instantiate` opens a scheme by replacing each quantified variable
//! with a fresh unification variable.

use super::env::TyEnv;
use super::scheme::Scheme;
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
use super::unify::{Unifier, Visited};
use std::collections::{HashMap, HashSet};

// ─────────────────────────────────────────────────────────────────────────────
// Free-variable collection
// ─────────────────────────────────────────────────────────────────────────────

/// All four variable kinds, collected in one traversal.
pub struct FreeVars {
    pub tys: HashSet<TyVar>,
    pub comps: HashSet<CompTyVar>,
    pub modes: HashSet<ModeVar>,
    pub rows: HashSet<RowVar>,
}

impl FreeVars {
    pub fn new() -> Self {
        Self {
            tys: HashSet::new(),
            comps: HashSet::new(),
            modes: HashSet::new(),
            rows: HashSet::new(),
        }
    }

    /// Pull cached residual free vars (`BTreeSet`, persisted form) into
    /// this `FreeVars` (`HashSet`, in-flight form).
    pub fn merge_cached(&mut self, cached: &super::scheme::CachedFreeVars) {
        self.tys.extend(&cached.ty_fv);
        self.comps.extend(&cached.comp_fv);
        self.modes.extend(&cached.mode_fv);
        self.rows.extend(&cached.row_fv);
    }

    /// Move all four sets into `target`, leaving `self` empty.
    pub fn merge_into(self, target: &mut Self) {
        target.tys.extend(self.tys);
        target.comps.extend(self.comps);
        target.modes.extend(self.modes);
        target.rows.extend(self.rows);
    }

    /// Drop the variables quantified by `s` from each set.  Companion to
    /// scheme instantiation, where the quantified vars are minted fresh.
    pub fn remove_quantified(&mut self, s: &Scheme) {
        for v in &s.ty_vars {
            self.tys.remove(v);
        }
        for v in &s.comp_ty_vars {
            self.comps.remove(v);
        }
        for v in &s.mode_vars {
            self.modes.remove(v);
        }
        for v in &s.row_vars {
            self.rows.remove(v);
        }
    }

    /// Intersect with `env` and persist as a [`CachedFreeVars`] — the
    /// "residual" free vars of a scheme, those visible in the
    /// surrounding environment and therefore not quantified.
    pub fn intersect_into_cached(&self, env: &Self) -> super::scheme::CachedFreeVars {
        super::scheme::CachedFreeVars {
            ty_fv: self.tys.intersection(&env.tys).copied().collect(),
            comp_fv: self.comps.intersection(&env.comps).copied().collect(),
            mode_fv: self.modes.intersection(&env.modes).copied().collect(),
            row_fv: self.rows.intersection(&env.rows).copied().collect(),
        }
    }
}

pub fn free_ty(u: &mut Unifier, ty: &Ty, out: &mut FreeVars) {
    let mut visited = Visited::default();
    free_ty_inner(u, ty, out, &mut visited);
}

fn free_ty_inner(u: &mut Unifier, ty: &Ty, out: &mut FreeVars, visited: &mut Visited) {
    // Set-discipline cycle guard: skipping a sibling revisit is correct
    // here because we are not modifying the unifier — the binding behind
    // the root is constant during traversal, so the first walk already
    // collected every free var reachable from it.
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
        // Ground types carry no variables.  Enumerating them means a new
        // `Ty` constructor with free vars fails the build here instead of
        // being silently dropped (which would under-generalise it).
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
    // Set-discipline cycle guard, same reasoning as `free_ty_inner`.
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
            // Unbound comp var — record it so generalize can quantify
            // over it and instantiate can mint fresh ids per use site.
            out.comps.insert(v);
        }
        CompTy::Return(spec, a) => {
            free_mode_inner(u, spec.input, out);
            free_mode_inner(u, spec.output, out);
            free_ty_inner(u, &a, out, visited);
        }
        CompTy::Fun(a, b) => {
            free_ty_inner(u, &a, out, visited);
            free_comp_inner(u, &b, out, visited);
        }
    }
}

fn free_mode_inner(u: &mut Unifier, mode: PipeMode, out: &mut FreeVars) {
    if let PipeMode::Var(v) = u.resolve_mode(&mode) {
        out.modes.insert(v);
    }
}

/// Collect free variables across all schemes in the environment.
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
    let mode_vars: Vec<ModeVar> = fvs.modes.difference(&env_fvs.modes).copied().collect();
    let row_vars: Vec<RowVar> = fvs.rows.difference(&env_fvs.rows).copied().collect();

    // Cache the residual free variables: those that appear in the environment
    // and were therefore NOT generalised.  For top-level bindings these are
    // all empty.  Stored so that future env_free_vars calls can skip traversal
    // for this scheme instead of re-walking the type tree.
    let residuals = fvs.intersect_into_cached(&env_fvs);
    // Each residual must be an unbound canonical root *in this unifier* at mint
    // time — the rescuing invariant for the cache outliving later `unite`/`bind`
    // calls is that every residual originates from a monomorphic (`Scheme::mono`)
    // environment binding that outlives every generalised scheme mentioning it,
    // so its var is never the one a later step moves or binds.  Holds by
    // construction here (a residual is drawn from `free_ty`'s output, which only
    // reports unbound canonical roots); the assertion makes the contract
    // explicit and guards a future change that derives residuals otherwise.
    #[allow(clippy::debug_assert_with_mut_call, reason = "the &mut is union-find path compression, semantically idempotent; skipping it in release is harmless")]
    {
        debug_assert!(
            residuals_are_live_roots(u, &residuals),
            "a generalisation residual is not an unbound canonical root — the cache \
             would go stale under a later unite/bind"
        );
    }
    let cached_fv = Some(residuals);

    // Snapshot any cyclic var bindings reachable from `applied`.  The
    // cycle-aware `apply_*` chain leaves back-edges as `CompTy::Var(root)`
    // / `Ty::Var(root)` nodes; collecting those roots and their bindings
    // lets `instantiate` mint fresh ids without sharing the original
    // union-find slot across instantiations.
    //
    // A cyclic root that is also free in the environment is monomorphic —
    // it must stay anchored at its original union-find slot so a constraint
    // from one use propagates to every other.  Freshening it per
    // instantiation (as the snapshot does for genuinely polymorphic roots)
    // would sever that sharing, so env-reachable roots are filtered out of
    // the snapshot, exactly as they are excluded from the plain quantifier
    // sets above.
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

    // Cyclic roots already appear in `*_bindings` — drop them from the
    // plain quantifier sets so they are not double-counted.
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
        mode_vars,
        row_vars,
        ty: applied,
        comp_ty_bindings,
        ty_bindings,
        cached_fv,
    }
}

/// Does every cached residual resolve to an unbound canonical root in `u`?
/// A residual that has been united (its root moved) or bound (resolves to a
/// non-`Var`) would make the cache silently disagree with the live unifier.
fn residuals_are_live_roots(u: &mut Unifier, residuals: &super::scheme::CachedFreeVars) -> bool {
    residuals
        .ty_fv
        .iter()
        .all(|v| u.ty_root(v.0) == v.0 && matches!(u.resolve_ty(&Ty::Var(*v)), Ty::Var(_)))
        && residuals.comp_fv.iter().all(|v| {
            u.comp_root(v.0) == v.0 && matches!(u.resolve_comp_ty(&CompTy::Var(*v)), CompTy::Var(_))
        })
        && residuals
            .mode_fv
            .iter()
            .all(|v| matches!(u.resolve_mode(&PipeMode::Var(*v)), PipeMode::Var(rv) if rv == *v))
        && residuals
            .row_fv
            .iter()
            .all(|v| matches!(u.resolve_row(&Row::Var(*v)), Row::Var(rv) if rv == *v))
}

/// A scheme is *closed* when every free variable in its body is either
/// quantified or captured as a cyclic-binding root.  The two scheme-minting
/// sites (`annotate`'s `Bind` rule, `alias_arm_scheme`) generalise against an
/// empty environment, so they must leave nothing free — an unquantified
/// residual would be a leaked open-scheme id that `Store::find`/`ensure`
/// silently tolerate as free and a later `fresh()` can re-mint and alias.
/// This makes that `schemes-leave-closed` invariant mechanical: a violation
/// trips the `debug_assert!` at the minting site rather than corrupting a
/// later inference run.
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
        && fvs.modes.iter().all(|v| scheme.mode_vars.contains(v))
        && fvs.rows.iter().all(|v| scheme.row_vars.contains(v))
}

/// Debug-only guard that a freshly minted scheme leaves no variable free.
/// Every empty-environment generalisation site (`annotate`'s `Bind` rule,
/// `alias_arm_scheme`, `binding_value_scheme`) must satisfy this closure
/// invariant; `msg` names the site so a violation trips there rather than
/// corrupting a later inference run.
#[allow(clippy::debug_assert_with_mut_call, reason = "the &mut is union-find path compression, semantically idempotent; skipping it in release is harmless")]
pub fn debug_assert_scheme_closed(u: &mut Unifier, scheme: &Scheme, msg: &str) {
    debug_assert!(scheme_is_closed(u, scheme), "{msg}");
}

pub fn instantiate(u: &mut Unifier, scheme: &Scheme) -> Ty {
    if !scheme.is_poly() {
        return scheme.ty.clone();
    }
    // Build comp-var and ty-var rename maps covering both the
    // non-cyclic quantified sets and the cyclic-binding roots.  Mints
    // a fresh union-find root per old id so two instantiations never
    // share state.
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
        tcm: tcm.clone(),
    };
    // Re-bind each fresh cyclic var root to the substituted binding so the
    // cycle survives instantiation but lives in fresh union-find slots.
    // Non-cyclic vars are left as fresh free roots.
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

/// Walk an applied type and collect the comp-var roots that appear as
/// `CompTy::Var(_)` back-edges, paired with each root's resolved binding
/// from the unifier.  Bindings are themselves applied (cycle-aware) so
/// that re-binding fresh roots to them at instantiation time produces a
/// detached copy of the cyclic structure.
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

/// Mirror of `snapshot_cyclic_comp_bindings` for value-type cycles.  The
/// canonical case is the streaming-consumer α := Variant {`more {head,
/// tail: Thunk(α)}, `done | ρ}.
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

/// Simultaneous substitution of type, mode, row, comp-ty, and cyclic
/// ty-root variables.  `cm` and `tcm` carry the mappings from old cyclic
/// roots to fresh ones — empty for non-recursive schemes.
struct SubstMap {
    tm: HashMap<TyVar, TyVar>,
    mm: HashMap<ModeVar, ModeVar>,
    rm: HashMap<RowVar, RowVar>,
    cm: HashMap<u32, u32>,
    tcm: HashMap<u32, u32>,
}

impl SubstMap {
    fn ty(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(TyVar(i)) => {
                // Cyclic-ty back-edge first: rewrite to the freshly minted
                // root id when the scheme captured this one as cyclic.
                if let Some(&fresh) = self.tcm.get(i) {
                    return Ty::Var(TyVar(fresh));
                }
                // Otherwise, plain quantified ty var.
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
            // Ground types carry no variables to rename.  Enumerating them
            // means a new `Ty` constructor with a quantifiable variable
            // fails the build here instead of being cloned unsubstituted
            // (instantiation capture).
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
                // Cyclic-comp back-edge: rewrite to the freshly minted
                // root id when the scheme captured this one as cyclic.
                let id = *self.cm.get(i).unwrap_or(i);
                CompTy::Var(CompTyVar(id))
            }
            CompTy::Return(spec, a) => CompTy::Return(
                PipeSpec {
                    input: self.mode(spec.input),
                    output: self.mode(spec.output),
                },
                Box::new(self.ty(a)),
            ),
            CompTy::Fun(a, b) => CompTy::Fun(Box::new(self.ty(a)), Box::new(self.comp(b))),
        }
    }

    fn mode(&self, mode: PipeMode) -> PipeMode {
        match mode {
            PipeMode::None | PipeMode::Bytes => mode,
            PipeMode::Var(v) => self.mm.get(&v).map_or(mode, |&f| PipeMode::Var(f)),
        }
    }
}
