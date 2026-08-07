//! Typing environment and inference context.

use super::error::{Reason, TypeError, TypeErrorKind};
use super::mode_solver::ModeConstraint;
use super::scheme::Scheme;
use super::ty::{CompTy, PipeMode, PipeSpec, Ty};
use super::unify::Unifier;
use crate::mode::ByteMode;
use crate::source::Span;
use std::collections::HashMap;

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
    /// The run's builtin table, seeded once by `seed_env`.  A name resolves
    /// here after a lexical binding and before a handler.
    pub builtins: crate::types::BuiltinTable,
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

    /// # Panics
    /// Panics if the scope stack is empty (more `pop`s than `push`es).
    pub fn bind(&mut self, name: String, scheme: Scheme) {
        self.scopes
            .last_mut()
            .unwrap()
            .bindings
            .insert(name, scheme);
    }

    /// # Panics
    /// Panics if the scope stack is empty (more `pop`s than `push`es).
    pub fn bind_handler(&mut self, name: String, scheme: Scheme, removable_by_unalias: bool) {
        self.scopes.last_mut().unwrap().handlers.insert(
            name,
            HandlerBinding {
                scheme,
                removable_by_unalias,
            },
        );
    }

    /// Remove a binding from whichever scope owns it.  `infer_letrec` drops its
    /// mono self-bindings before generalising: left in place, their free vars
    /// read as environment residuals and block quantification.
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

/// Side tables inference fills and the annotation pass reads back.  Every map is
/// keyed by node address, so both passes must walk the very same live tree; a
/// clone between them silently misses.
pub struct InferCtx {
    pub unifier: Unifier,
    pub errors: Vec<TypeError>,
    /// Source position for newly emitted [`TypeError`]s, narrowed by `with_span`.
    pub pos: Option<Span>,
    /// Pre-generalisation type bound by each `Name`-pattern `Bind`.
    pub bind_tys: HashMap<usize, Ty>,
    /// Each stage's `F[I,O]`, settled only once `infer_pipeline` has unified every
    /// adjacency; its modes may still be variables until grounded to a
    /// [`crate::mode::Wire`].
    pub stage_specs: HashMap<usize, PipeSpec>,
    /// The value flowing out of each pipeline stage.  Feeds the structural REPL's
    /// typed spine; the evaluator never reads it.
    pub stage_types: HashMap<usize, Ty>,
    /// A `Comp` node's own top-level `result`, recorded for `annotate`'s
    /// demand walk; absent = no `Return` shape at record time = `∅`.
    pub results: HashMap<usize, PipeMode>,
    /// A scope arm's (`Val`-keyed) own `result`, the `Val`-level analogue of
    /// [`Self::results`] — scope arms have no `Comp` node of their own.
    pub val_results: HashMap<usize, PipeMode>,
    /// Joins and arm-result merges not yet determined, awaiting
    /// [`Self::solve_and_finalize`](super::mode_solver); see `mode_solver`.
    pub(super) mode_constraints: Vec<ModeConstraint>,
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
            bind_tys: HashMap::new(),
            stage_specs: HashMap::new(),
            stage_types: HashMap::new(),
            results: HashMap::new(),
            val_results: HashMap::new(),
            mode_constraints: Vec::new(),
        }
    }

    /// Ground a mode into a [`ByteMode`]; a still-unresolved variable defaults empty.
    pub fn ground(&mut self, mode: PipeMode) -> ByteMode {
        match self.unifier.resolve_mode(&mode) {
            PipeMode::Bytes => ByteMode::Bytes,
            PipeMode::None | PipeMode::Var(_) => ByteMode::Empty,
        }
    }

    /// Push a diagnosis that is its own story, with no constraint provenance.
    pub fn diagnose(&mut self, kind: TypeErrorKind) {
        self.errors.push(TypeError {
            pos: self.pos,
            kind,
            reason: None,
        });
    }

    /// Push a constraint failure with its provenance.
    pub fn report(&mut self, kind: TypeErrorKind, why: Reason) {
        self.errors.push(TypeError {
            pos: self.pos,
            kind,
            reason: Some(why),
        });
    }

    /// Unify two value types, reporting a mismatch under `why`.
    pub fn unify_ty(&mut self, a: &Ty, b: &Ty, why: Reason) {
        if let Err(kind) = self.unifier.unify_ty(a, b) {
            self.report(kind, why);
        }
    }

    /// Unify two computation types, reporting a mismatch under `why`.
    pub fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy, why: Reason) {
        if let Err(kind) = self.unifier.unify_comp_ty(a, b) {
            self.report(kind, why);
        }
    }

    /// Unify two pipeline modes, reporting a `ModeMismatch` under `why`.
    pub fn unify_mode(&mut self, a: &PipeMode, b: &PipeMode, why: Reason) {
        if let Err(m) = self.unifier.unify_mode(a, b) {
            self.report(
                TypeErrorKind::ModeMismatch {
                    expected: m.left,
                    actual: m.right,
                },
                why,
            );
        }
    }
}
