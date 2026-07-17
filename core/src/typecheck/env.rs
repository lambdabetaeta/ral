//! Typing environment and inference context.

use super::error::{Reason, TypeError, TypeErrorKind};
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
    /// The checked session's builtin table — names a `Bind`/`Exec`/`Val`
    /// site resolves against before falling through to a lexical binding
    /// or handler.  Set once by `seed_env`; unchanged for the rest of the
    /// turn's inference.
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
///
/// read by `annotate` before any node is freed; a clone between the two
/// passes would silently miss.
pub struct InferCtx {
    pub unifier: Unifier,
    pub errors: Vec<TypeError>,
    /// Current source position for newly emitted [`TypeError`]s.
    /// Narrowed scopewise by [`Inferencer::with_span`](crate::source::WithSpan::with_span) (in
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
    /// annotation pass, which grounds each spec into a [`crate::mode::Wire`].  The
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
        Self {
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

    /// Push a direct diagnosis — a structured kind that is its own story,
    /// with no constraint provenance.
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

    /// Unify two value types, reporting a constraint failure under `why` on mismatch.
    pub fn unify_ty(&mut self, a: &Ty, b: &Ty, why: Reason) {
        if let Err(kind) = self.unifier.unify_ty(a, b) {
            self.report(kind, why);
        }
    }

    /// Unify two computation types, reporting a constraint failure under `why` on mismatch.
    pub fn unify_comp_ty(&mut self, a: &CompTy, b: &CompTy, why: Reason) {
        if let Err(kind) = self.unifier.unify_comp_ty(a, b) {
            self.report(kind, why);
        }
    }

    /// Unify two pipeline modes, reporting a `ModeMismatch` under `why` on mismatch.
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
