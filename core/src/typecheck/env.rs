//! Typing environment and inference context.

use super::error::{Reason, TypeError, TypeErrorKind, UnitCall};
use super::index::{Idx, Lbl};
use super::kind::Kind;
use super::scheme::Scheme;
use super::ty::{CompTy, Ty};
use super::unify::{Unifier, WeakSource};
use crate::source::Span;
use crate::types::{Fixings, Site};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Typing environment
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct HandlerBinding {
    pub(crate) scheme: Arc<Scheme>,
    pub(crate) removable_by_unalias: bool,
}

#[derive(Clone, Default)]
struct NameScope {
    bindings: HashMap<String, Arc<Scheme>>,
    /// For a binding `let` made of what a call returned, the callee.
    called: HashMap<String, String>,
    handlers: HashMap<String, HandlerBinding>,
}

#[derive(Clone)]
pub struct TyEnv {
    scopes: Vec<NameScope>,
    /// The run's builtin table, seeded once by `seed_env`.  A name resolves
    /// here after a lexical binding and before a handler.
    pub(crate) builtins: crate::types::BuiltinTable,
}

impl Default for TyEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl TyEnv {
    pub fn new() -> Self {
        Self {
            scopes: vec![NameScope::default()],
            builtins: crate::types::BuiltinTable::default(),
        }
    }

    pub(crate) fn lookup_binding(&self, name: &str) -> Option<&Arc<Scheme>> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
    }

    /// The callee of the call the innermost binding of `name` was made of.
    pub(crate) fn lookup_called(&self, name: &str) -> Option<&str> {
        self.scopes
            .iter()
            .rev()
            .find(|scope| scope.bindings.contains_key(name))
            .and_then(|scope| scope.called.get(name))
            .map(String::as_str)
    }

    pub(crate) fn binding_names(&self) -> impl Iterator<Item = &str> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.bindings.keys().map(String::as_str))
    }

    pub(crate) fn lookup_handler(&self, name: &str) -> Option<&HandlerBinding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.handlers.get(name))
    }

    pub fn push(&mut self) {
        self.scopes.push(NameScope::default());
    }
    pub(crate) fn pop(&mut self) {
        self.scopes.pop();
    }

    /// # Panics
    /// Panics if the scope stack is empty (more `pop`s than `push`es).
    pub(crate) fn bind(&mut self, name: String, scheme: impl Into<Arc<Scheme>>) {
        let scope = self.scopes.last_mut().unwrap();
        scope.called.remove(&name);
        scope.bindings.insert(name, scheme.into());
    }

    /// Note that the binding of `name` in the innermost scope is what `callee`
    /// returned.
    ///
    /// # Panics
    /// Panics if the scope stack is empty (more `pop`s than `push`es).
    pub(crate) fn note_called(&mut self, name: String, callee: String) {
        self.scopes.last_mut().unwrap().called.insert(name, callee);
    }

    /// # Panics
    /// Panics if the scope stack is empty (more `pop`s than `push`es).
    pub(crate) fn bind_handler(
        &mut self,
        name: String,
        scheme: impl Into<Arc<Scheme>>,
        removable_by_unalias: bool,
    ) {
        self.scopes.last_mut().unwrap().handlers.insert(
            name,
            HandlerBinding {
                scheme: scheme.into(),
                removable_by_unalias,
            },
        );
    }

    /// Remove a binding from whichever scope owns it.  The `Rec` group
    /// inference drops its mono self-bindings before generalising: left in
    /// place, their free vars read as environment residuals and block
    /// quantification.
    pub(crate) fn unbind(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if scope.bindings.remove(name).is_some() {
                return;
            }
        }
    }

    pub(crate) fn unbind_removable_handler(&mut self, name: &str) -> bool {
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

    pub(crate) fn all_schemes(&self) -> impl Iterator<Item = &Scheme> {
        self.scopes.iter().flat_map(|s| {
            s.bindings
                .values()
                .chain(s.handlers.values().map(|handler| &handler.scheme))
                .map(Arc::as_ref)
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Inference context
// ─────────────────────────────────────────────────────────────────────────────

/// The key of a node in [`InferCtx`]'s side tables: its address in the one
/// live tree both passes walk.
pub(super) fn comp_key(comp: &crate::ir::Comp) -> usize {
    std::ptr::from_ref::<crate::ir::Comp>(comp) as usize
}

pub(super) fn val_key(val: &crate::ir::Val) -> usize {
    std::ptr::from_ref::<crate::ir::Val>(val) as usize
}

/// A boundary builtin used as a value: what `annotate` rebuilds into the block
/// `{ |x…| name x… }`, whose call carries the site.
pub(super) struct BoundaryValue {
    pub(super) name: String,
    pub(super) arity: usize,
}

/// Side tables inference fills and the annotation pass reads back.  Every map is
/// keyed by node address, so both passes must walk the very same live tree; a
/// clone between them silently misses.
pub struct InferCtx {
    pub(crate) unifier: Unifier,
    pub(crate) errors: Vec<TypeError>,
    /// Source position for newly emitted [`TypeError`]s, narrowed by `with_span`.
    pub pos: Option<Span>,
    /// The computations a value demand captures, keyed by node address:
    /// `annotate` wraps each in `cap … to d. decode d`.
    pub(crate) captured: HashSet<usize>,
    /// The values in hand a value demand captures — a command block passed
    /// where a value producer was wanted — keyed by address, with the arity
    /// `annotate` η-wraps them at.
    pub(crate) captured_vals: HashMap<usize, usize>,
    /// The value flowing out of each pipeline stage.  Feeds the structural REPL's
    /// typed spine; the evaluator never reads it.
    pub(crate) stage_types: HashMap<usize, Ty>,
    /// Each read of a name bound to what a call returned, which was `()`, at the
    /// span it was read at: an error raised there says why the name is `()`.
    pub(super) unit_reads: Vec<(Span, UnitCall)>,
    /// Label reads whose target's head is not yet known.
    pub(super) pending_labels: Vec<Lbl>,
    /// Computed reads whose target's head is not yet known.
    pub(super) pending_indexes: Vec<Idx>,
    /// The binding whose value is being inferred, narrowed like [`Self::pos`].
    pub(super) holder: Option<Span>,
    /// A `Rec` group's member types, inferred once per `Arc` within a run
    /// and keyed by its identity — every projection of the same group reads
    /// the same betas rather than re-inferring the group.
    pub(crate) rec_groups: HashMap<*const (), Vec<CompTy>>,
    /// `annotate`'s rebuilt `GroupNode` per source group's identity, so every
    /// `Rec` projection of one group shares the one node the elaborator
    /// built, rather than each rebuilding its own.
    pub(crate) rec_group_rebuilds: HashMap<*const (), Arc<crate::ir::GroupNode>>,
    /// A `Bind`/`Define`/tail-`Run` RHS's curried arity, recorded whenever
    /// its inferred type resolved to `Fun` — keyed by the RHS node's own
    /// address, read back by `annotate`'s η-expansion.
    pub(crate) rhs_arrow_arity: HashMap<usize, usize>,
    /// The result type of each boundary call, keyed by the `Exec` node's
    /// address, and of each boundary builtin used as a value, keyed by its
    /// `Val`'s; frozen into a [`Site`] once the unit is solved.
    pub(super) boundary_results: HashMap<usize, Ty>,
    /// What `annotate` rebuilds a boundary builtin used as a value into: a
    /// block holding the saturated call, keyed like [`Self::boundary_results`].
    pub(super) boundary_values: HashMap<usize, BoundaryValue>,
    /// How many arguments an under-applied boundary call lacks, keyed by its
    /// `Exec`: `annotate` η-expands it to a saturated call, where the site is.
    pub(super) boundary_missing: HashMap<usize, usize>,
    /// The sites [`Self::snapshot_sites`] froze, under the same keys.
    pub(super) sites: HashMap<usize, Arc<Site>>,
    /// Stored session bindings that entered this unit with weak residuals, as
    /// seeded.
    pub(super) residuals: Vec<(String, Arc<Scheme>)>,
    /// The type of the first use, in this unit, of each of [`Self::residuals`].
    residual_uses: Vec<(String, Ty)>,
    /// The sites a data binding with residuals is re-admitted against before
    /// the unit runs.
    pub(crate) readmits: Vec<(String, Arc<Site>)>,
    /// Fresh-name counter for compiler-synthesized binders (η-expansion's
    /// parameters, the decode coercion's bind).
    synth_counter: usize,
}

impl Default for InferCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl InferCtx {
    pub fn new() -> Self {
        Self {
            unifier: Unifier::new(),
            errors: Vec::new(),
            pos: None,
            captured: HashSet::new(),
            captured_vals: HashMap::new(),
            stage_types: HashMap::new(),
            unit_reads: Vec::new(),
            pending_labels: Vec::new(),
            pending_indexes: Vec::new(),
            holder: None,
            rec_groups: HashMap::new(),
            rec_group_rebuilds: HashMap::new(),
            rhs_arrow_arity: HashMap::new(),
            boundary_results: HashMap::new(),
            boundary_values: HashMap::new(),
            boundary_missing: HashMap::new(),
            sites: HashMap::new(),
            residuals: Vec::new(),
            residual_uses: Vec::new(),
            readmits: Vec::new(),
            synth_counter: 0,
        }
    }

    /// A fresh, distinctive name for a compiler-synthesized binder, tagged
    /// with `purpose` (`"eta"`, `"decode"`) — never written by any surface
    /// program, so no collision guard is needed beyond the counter itself.
    pub(crate) fn fresh_name(&mut self, purpose: &str) -> String {
        self.synth_counter += 1;
        format!("__{purpose}{}", self.synth_counter)
    }

    /// Record `ty` as the result of the boundary node at `key`, and make what
    /// is free in it weak: its one type for the unit is what its door admits
    /// the value against.
    pub(super) fn record_boundary(&mut self, key: usize, name: &str, ty: Ty) {
        self.unifier
            .mark_weak(&ty, &WeakSource::Boundary(name.into()));
        self.boundary_results.insert(key, ty);
    }

    /// Note that the stored binding `name`, read at `scheme`, is used here at
    /// `ty`: the first such use is what the binding is re-admitted against.
    pub(super) fn note_residual_use(&mut self, name: &str, scheme: &Arc<Scheme>, ty: &Ty) {
        let residual = self
            .residuals
            .iter()
            .any(|(n, stored)| n == name && Arc::ptr_eq(stored, scheme));
        if residual && !self.residual_uses.iter().any(|(n, _)| n == name) {
            self.residual_uses.push((name.to_owned(), ty.clone()));
        }
    }

    /// Freeze every recorded boundary, and every residual use, as the unit's
    /// solution has left it.  One [`Fixings`] is shared by all of them.
    pub(super) fn snapshot_sites(&mut self) {
        let fixings = Fixings::new();
        let freeze = |ty: &Ty| Arc::new(Site::snapshot(&self.unifier, ty, Arc::clone(&fixings)));
        self.sites = self
            .boundary_results
            .iter()
            .map(|(&key, ty)| (key, freeze(ty)))
            .collect();
        self.readmits = self
            .residual_uses
            .iter()
            .map(|(name, ty)| (name.clone(), freeze(ty)))
            .collect();
    }

    /// Push a diagnosis that is its own story, with no constraint provenance.
    pub(crate) fn diagnose(&mut self, kind: TypeErrorKind) {
        self.raise(TypeError {
            pos: self.pos,
            kind,
            reason: None,
            weak: None,
            unit: None,
        });
    }

    /// Record `error` unless the same sentence is already said at the same
    /// span: two operands at one type, both refused, are one complaint.
    pub(super) fn raise(&mut self, mut error: TypeError) {
        error.unit = error.pos.and_then(|pos| {
            self.unit_reads
                .iter()
                .find(|(read, _)| *read == pos)
                .map(|(_, call)| call.clone())
        });
        let said = self
            .errors
            .iter()
            .any(|e| e.pos == error.pos && e.kind.render_message() == error.kind.render_message());
        if !said {
            self.errors.push(error);
        }
    }

    /// Push a constraint failure with its provenance.
    pub(crate) fn report(&mut self, kind: TypeErrorKind, why: Reason) {
        self.report_weak(kind, why, None);
    }

    /// [`report`](Self::report), for a constraint that met a weak variable when `weak`.
    fn report_weak(&mut self, kind: TypeErrorKind, why: Reason, weak: Option<WeakSource>) {
        self.raise(TypeError {
            pos: self.pos,
            kind,
            reason: Some(why),
            weak,
            unit: None,
        });
    }

    /// A fresh variable of `kind`, narrowed at the span in force.
    pub(crate) fn fresh_kinded(&mut self, kind: Kind) -> Ty {
        self.unifier.at = self.pos;
        self.unifier.fresh_kinded(kind)
    }

    /// Instantiate `scheme`, each fresh variable's kind narrowed at the span
    /// in force.
    pub(crate) fn instantiate(&mut self, scheme: &Scheme) -> Ty {
        self.unifier.at = self.pos;
        super::generalize::instantiate(&mut self.unifier, scheme)
    }

    /// Unify two value types, reporting a mismatch under `why`.
    pub(crate) fn unify_ty(&mut self, a: &Ty, b: &Ty, why: Reason) {
        self.unifier.at = self.pos;
        if let Err(kind) = self.unifier.unify_ty(a, b) {
            let weak = self
                .unifier
                .weak_source_in_ty(a)
                .or_else(|| self.unifier.weak_source_in_ty(b));
            self.report_weak(kind, why, weak);
        }
    }

    /// Unify two computation types, reporting a mismatch under `why`.
    pub(crate) fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy, why: Reason) {
        self.unifier.at = self.pos;
        if let Err(kind) = self.unifier.unify_comp_ty(a, b) {
            let weak = self
                .unifier
                .weak_source_in_comp(a)
                .or_else(|| self.unifier.weak_source_in_comp(b));
            self.report_weak(kind, why, weak);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The second use meets `Int`, which the weak variable was fixed to, not
    /// the variable itself.
    #[test]
    fn a_mismatch_with_what_a_weak_variable_was_fixed_to_carries_the_note() {
        let mut ctx = InferCtx::new();
        let weak = ctx.unifier.fresh_ty();
        ctx.unifier.mark_weak(&weak, &WeakSource::Index);
        ctx.unify_ty(&weak, &Ty::Int, Reason::Argument);
        ctx.unify_ty(&weak, &Ty::String, Reason::Argument);
        let [err] = &ctx.errors[..] else {
            panic!("one mismatch expected: {:?}", ctx.errors);
        };
        assert_eq!(err.weak, Some(WeakSource::Index));
        assert!(
            err.hint()
                .is_some_and(|h| h.contains("another use fixed it"))
        );
    }

    #[test]
    fn a_mismatch_with_no_weak_variable_has_no_note() {
        let mut ctx = InferCtx::new();
        let var = ctx.unifier.fresh_ty();
        ctx.unify_ty(&var, &Ty::Int, Reason::Argument);
        ctx.unify_ty(&var, &Ty::String, Reason::Argument);
        let [err] = &ctx.errors[..] else {
            panic!("one mismatch expected: {:?}", ctx.errors);
        };
        assert_eq!(err.weak, None);
        assert!(
            err.hint()
                .is_some_and(|h| !h.contains("another use fixed it"))
        );
    }
}
