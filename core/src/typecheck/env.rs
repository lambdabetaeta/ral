//! Typing environment and inference context.

use super::contract::NestedCheck;
use super::error::{Clash, Reason, TypeError, TypeErrorKind, UnitCall};
use super::unify::{Unifier, WeakSource};
use crate::ir::{GroupNode, Name, synthetic};
use crate::source::Span;
use crate::ty::Site;
use crate::ty::{CompTy, Kind, Scheme, Ty};
use std::collections::HashMap;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Typing environment
// ─────────────────────────────────────────────────────────────────────────────

/// What installed a handler: `unalias` removes only an [`Self::Alias`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandlerOrigin {
    Within,
    Alias,
}

#[derive(Clone)]
pub(crate) struct HandlerBinding {
    pub(crate) scheme: Arc<Scheme>,
    pub(crate) origin: HandlerOrigin,
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
    /// The run's Σ, seeded once by `seed_env`.  A name resolves here after a
    /// lexical binding and before a handler.
    pub(crate) builtins: super::builtins::Manifest,
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
            builtins: super::builtins::Manifest::default(),
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

    /// Only [`Inferencer::with_scope`](super::infer::Inferencer::with_scope)
    /// opens and closes a scope.
    pub(super) fn push(&mut self) {
        self.scopes.push(NameScope::default());
    }

    pub(super) fn pop(&mut self) {
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
        origin: HandlerOrigin,
    ) {
        self.scopes.last_mut().unwrap().handlers.insert(
            name,
            HandlerBinding {
                scheme: scheme.into(),
                origin,
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
                Some(binding) if binding.origin == HandlerOrigin::Alias
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

/// A site as inference leaves it, frozen once the unit succeeds.
pub(super) enum Pending {
    /// Preserve a checked tree's site, including its unit identity.
    Kept(Arc<Site>),
    /// Snapshot this unit's solved type.
    Open(Ty),
    /// Only on `Val::Protected`; keep the membrane only for delayed parts.
    Imported(Ty),
}

/// Per-unit inference state and obligations awaiting successful closure.
pub struct InferCtx {
    pub(crate) unifier: Unifier,
    pub(crate) errors: Vec<TypeError>,
    /// Source position for new diagnostics, narrowed by `with_span`.
    pub pos: Option<Span>,
    /// Diagnostic provenance for reads of names bound to unit-valued calls.
    pub(super) unit_reads: Vec<(Span, UnitCall)>,
    /// Nested option tables awaiting ascription after inference.
    pub(super) nested: Vec<NestedCheck>,
    /// All projections share one inferred group.
    pub(super) rec_groups: HashMap<*const GroupNode, (Vec<CompTy>, Arc<GroupNode<Pending>>)>,
    /// Imported residual schemes, identified by their stored identity.
    pub(super) residuals: Vec<(String, Arc<Scheme>)>,
    /// Frozen into `Toplevel.admits` in first-use order.
    pub(super) residual_uses: Vec<(String, Ty)>,
    /// Unit-local freshness for compiler-written binders.
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
            unit_reads: Vec::new(),
            nested: Vec::new(),
            rec_groups: HashMap::new(),
            residuals: Vec::new(),
            residual_uses: Vec::new(),
            synth_counter: 0,
        }
    }

    /// A fresh compiler-written binder tagged with `purpose` (`"eta"`,
    /// `"decode"`).
    pub(crate) fn fresh_name(&mut self, purpose: &str) -> Name {
        self.synth_counter += 1;
        synthetic(purpose, self.synth_counter)
    }

    /// Mark a boundary's result weak.
    pub(super) fn record_boundary(&mut self, name: &str, ty: &Ty) {
        self.unifier
            .mark_weak(ty, &WeakSource::Boundary(name.into()));
    }

    /// Record an imported binding's first use.
    pub(super) fn note_residual_use(&mut self, name: &str, scheme: &Arc<Scheme>, ty: &Ty) -> bool {
        let residual = self
            .residuals
            .iter()
            .any(|(n, stored)| n == name && Arc::ptr_eq(stored, scheme));
        if residual && !self.residual_uses.iter().any(|(n, _)| n == name) {
            self.residual_uses.push((name.to_owned(), ty.clone()));
        }
        residual
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
        self.unify_ty_with(a, b, |u, a, b| {
            u.unify_ty(a, b).map_err(|kind| Box::new((kind, why)))
        });
    }

    /// [`Self::unify_ty`] by `unify`, which words its own failure: a reason
    /// that depends on where in the types the clash fell.
    pub(super) fn unify_ty_with(
        &mut self,
        a: &Ty,
        b: &Ty,
        unify: impl FnOnce(&mut Unifier, &Ty, &Ty) -> Result<(), Clash>,
    ) {
        self.unifier.at = self.pos;
        if let Err(clash) = unify(&mut self.unifier, a, b) {
            let (kind, why) = *clash;
            let weak = self
                .unifier
                .weak_source_in_ty(a)
                .or_else(|| self.unifier.weak_source_in_ty(b));
            self.report_weak(kind, why, weak);
        }
    }

    /// Unify two value types at the span in force, handing a mismatch back
    /// rather than reporting it.
    pub(crate) fn try_unify_ty(&mut self, a: &Ty, b: &Ty) -> Result<(), TypeErrorKind> {
        self.unifier.at = self.pos;
        self.unifier.unify_ty(a, b)
    }

    /// Unify two computation types at the span in force, handing a mismatch
    /// back rather than reporting it.
    pub(crate) fn try_unify_comp_ty(
        &mut self,
        a: &CompTy,
        b: &CompTy,
    ) -> Result<(), TypeErrorKind> {
        self.unifier.at = self.pos;
        self.unifier.unify_comp_ty(a, b)
    }

    /// Unify two computation types, reporting a mismatch under `why`.
    pub(crate) fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy, why: Reason) {
        if let Err(kind) = self.try_unify_comp_ty(a, b) {
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
    use crate::source::FileId;

    /// The second use meets `Int`, which the weak variable was fixed to, not
    /// the variable itself.
    #[test]
    fn a_mismatch_with_what_a_weak_variable_was_fixed_to_carries_the_note() {
        let mut ctx = InferCtx::new();
        let weak = ctx.unifier.fresh_ty();
        ctx.unifier
            .mark_weak(&weak, &WeakSource::Boundary("from-json".into()));
        ctx.unify_ty(&weak, &Ty::Int, Reason::Argument);
        ctx.unify_ty(&weak, &Ty::String, Reason::Argument);
        let [err] = &ctx.errors[..] else {
            panic!("one mismatch expected: {:?}", ctx.errors);
        };
        assert_eq!(err.weak, Some(WeakSource::Boundary("from-json".into())));
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

    /// One sentence is said once per span: a repeat is dropped, a new span or a
    /// new sentence is kept.
    #[test]
    fn raise_drops_only_the_same_sentence_at_the_same_span() {
        let mismatch = |actual| TypeErrorKind::TyMismatch {
            expected: Box::new(Ty::Int),
            actual: Box::new(actual),
        };
        let at = |start| Some(Span::new(FileId::DUMMY, start, start + 1));
        let mut ctx = InferCtx::new();
        for (pos, ty) in [
            (at(0), Ty::String),
            (at(0), Ty::String),
            (at(2), Ty::String),
            (at(0), Ty::Bool),
        ] {
            ctx.pos = pos;
            ctx.diagnose(mismatch(ty));
        }
        let kept: Vec<_> = ctx.errors.iter().map(|e| e.pos).collect();
        assert_eq!(kept, [at(0), at(2), at(0)]);
    }
}
