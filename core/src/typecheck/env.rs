//! Typing environment and inference context, plus free-variable collection.

use super::scheme::{Scheme, TypeError, TypeErrorKind};
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
use super::unify::{Unifier, Visited};
use crate::mode::ByteMode;
use crate::source::Span;
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
        FreeVars {
            tys: HashSet::new(),
            comps: HashSet::new(),
            modes: HashSet::new(),
            rows: HashSet::new(),
        }
    }

    /// Pull cached residual free vars (BTreeSet, persisted form) into
    /// this `FreeVars` (HashSet, in-flight form).
    pub fn merge_cached(&mut self, cached: &super::scheme::CachedFreeVars) {
        self.tys.extend(&cached.ty_fv);
        self.comps.extend(&cached.comp_fv);
        self.modes.extend(&cached.mode_fv);
        self.rows.extend(&cached.row_fv);
    }

    /// Move all four sets into `target`, leaving `self` empty.
    pub fn merge_into(self, target: &mut FreeVars) {
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
    pub fn intersect_into_cached(&self, env: &FreeVars) -> super::scheme::CachedFreeVars {
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
            free_mode_inner(u, &spec.input, out);
            free_mode_inner(u, &spec.output, out);
            free_ty_inner(u, &a, out, visited);
        }
        CompTy::Fun(a, b) => {
            free_ty_inner(u, &a, out, visited);
            free_comp_inner(u, &b, out, visited);
        }
    }
}

fn free_mode_inner(u: &mut Unifier, mode: &PipeMode, out: &mut FreeVars) {
    if let PipeMode::Var(v) = u.resolve_mode(mode) {
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

// ─────────────────────────────────────────────────────────────────────────────
// Typing environment
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct HandlerBinding {
    pub scheme: Scheme,
    pub removable_by_unalias: bool,
}

#[derive(Clone, Default)]
struct NameScope {
    bindings: HashMap<String, Scheme>,
    handlers: HashMap<String, HandlerBinding>,
}

#[derive(Clone)]
pub struct TyEnv {
    scopes: Vec<NameScope>,
}

impl Default for TyEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl TyEnv {
    pub fn new() -> Self {
        TyEnv {
            scopes: vec![NameScope::default()],
        }
    }

    pub fn lookup_binding(&self, name: &str) -> Option<&Scheme> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
    }

    pub fn lookup_handler(&self, name: &str) -> Option<&HandlerBinding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.handlers.get(name))
    }

    pub fn push(&mut self) {
        self.scopes.push(NameScope::default());
    }
    pub fn pop(&mut self) {
        self.scopes.pop();
    }

    pub fn bind(&mut self, name: String, scheme: Scheme) {
        self.scopes
            .last_mut()
            .unwrap()
            .bindings
            .insert(name, scheme);
    }

    pub fn bind_handler(&mut self, name: String, scheme: Scheme, removable_by_unalias: bool) {
        self.scopes.last_mut().unwrap().handlers.insert(
            name,
            HandlerBinding {
                scheme,
                removable_by_unalias,
            },
        );
    }

    /// Remove a binding from whichever scope owns it.  Used by
    /// `infer_letrec` to drop the temporary mono self-binding before
    /// generalising — leaving it in place would let its free comp
    /// vars leak into `env_free_vars` as residuals, blocking
    /// quantification of exactly the vars we need to quantify over.
    pub fn unbind(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if scope.bindings.remove(name).is_some() {
                return;
            }
        }
    }

    pub fn unbind_removable_handler(&mut self, name: &str) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if matches!(
                scope.handlers.get(name),
                Some(binding) if binding.removable_by_unalias
            ) {
                scope.handlers.remove(name);
                return true;
            }
        }
        false
    }

    pub fn all_schemes(&self) -> impl Iterator<Item = &Scheme> {
        self.scopes.iter().flat_map(|s| {
            s.bindings
                .values()
                .chain(s.handlers.values().map(|handler| &handler.scheme))
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Inference context
// ─────────────────────────────────────────────────────────────────────────────

/// The `bind_tys`/`stage_specs`/`bind_outputs` maps are keyed by node
/// address and are valid only for the exact tree passed to `infer_comp`,
/// read by `annotate` before any node is freed; a clone between the two
/// passes would silently miss.
pub struct InferCtx {
    pub unifier: Unifier,
    pub errors: Vec<TypeError>,
    /// Current source position for newly emitted [`TypeError`]s.
    /// Narrowed scopewise by [`Inferencer::with_span`] (in
    /// `typecheck/infer.rs`), the single per-position narrowing
    /// primitive the inferencer uses.
    pub pos: Option<Span>,
    /// Pre-generalisation bound types of `Name`-pattern `Bind` nodes,
    /// keyed by node address.  Written by the `Bind` rule in
    /// `infer.rs`, read by the annotation pass, which resolves each
    /// against the final unifier and quantifies the residuals.
    pub bind_tys: HashMap<usize, Ty>,
    /// The `F[input, output]` spec inferred for each pipeline stage,
    /// keyed by the stage `Comp`'s address.  Written at the end of
    /// `infer_pipeline` after every adjacency is unified; read by the
    /// annotation pass, which grounds each spec into a [`Wire`].  The
    /// recorded modes may still be variables — they resolve at
    /// annotation time, after the whole walk.
    pub stage_specs: HashMap<usize, PipeSpec>,
    /// The inferred *value* type of each pipeline stage — the data flowing
    /// out of it — keyed by the stage `Comp`'s address.  Written at the end
    /// of `infer_pipeline` alongside `stage_specs`; read by the annotation
    /// pass, which resolves each against the final unifier and writes the
    /// `Vec<Ty>` onto the `Pipeline` node.  Retained for the structural
    /// REPL's typed spine; the evaluator never reads it.
    pub stage_types: HashMap<usize, Ty>,
    /// The output mode of each `Bind` node's RHS, keyed by the `Bind`
    /// `Comp`'s address.  Written by the `Bind` rule for every pattern;
    /// read by the annotation pass, which grounds it into the node's
    /// `rhs_output` `ByteMode`.
    pub bind_outputs: HashMap<usize, PipeMode>,
    /// The output mode of the final computation whose value a node returns.
    /// A `Seq` may write bytes before its last statement; those bytes are
    /// effects, not the returned value's byte source.  This map keeps the two
    /// facts separate for `let` and `try` value-boundary typing.
    pub final_outputs: HashMap<usize, PipeMode>,
}

impl Default for InferCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl InferCtx {
    pub fn new() -> Self {
        InferCtx {
            unifier: Unifier::new(),
            errors: Vec::new(),
            pos: None,
            bind_tys: HashMap::new(),
            stage_specs: HashMap::new(),
            stage_types: HashMap::new(),
            bind_outputs: HashMap::new(),
            final_outputs: HashMap::new(),
        }
    }

    /// Ground a pipeline mode into a [`ByteMode`], resolving it through
    /// the final unifier and defaulting an unresolved variable to the
    /// `∅` channel — the single defaulting rule applied at the
    /// unification frontier.
    pub fn ground(&mut self, mode: PipeMode) -> ByteMode {
        match self.unifier.resolve_mode(&mode) {
            PipeMode::Bytes => ByteMode::Bytes,
            PipeMode::None | PipeMode::Var(_) => ByteMode::Empty,
        }
    }

    pub fn error_hint(&mut self, msg: String, hint: &str) {
        self.emit_kind(TypeErrorKind::AdHoc { message: msg }, Some(hint));
    }

    /// Push a type error from the unifier or inferencer.
    pub fn emit_kind(&mut self, kind: TypeErrorKind, hint: Option<&str>) {
        self.errors.push(TypeError {
            pos: self.pos,
            kind,
            hint: hint.map(|s| s.to_string()),
        });
    }

    pub fn unify_ty(&mut self, a: &Ty, b: &Ty) {
        if let Err(kind) = self.unifier.unify_ty(a, b) {
            self.emit_kind(kind, None);
        }
    }

    pub fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy) {
        if let Err(kind) = self.unifier.unify_comp_ty(a, b) {
            self.emit_kind(kind, None);
        }
    }

    pub fn unify_mode(&mut self, a: &PipeMode, b: &PipeMode) {
        if let Err(m) = self.unifier.unify_mode(a, b) {
            self.emit_kind(
                TypeErrorKind::ModeMismatch {
                    expected: m.left,
                    actual: m.right,
                },
                None,
            );
        }
    }

    pub fn unify_ty_hint(&mut self, a: &Ty, b: &Ty, hint: &str) {
        if let Err(kind) = self.unifier.unify_ty(a, b) {
            self.emit_kind(kind, Some(hint));
        }
    }

    pub fn unify_comp_ty_hint(&mut self, a: &CompTy, b: &CompTy, hint: &str) {
        if let Err(kind) = self.unifier.unify_comp_ty(a, b) {
            self.emit_kind(kind, Some(hint));
        }
    }
}
