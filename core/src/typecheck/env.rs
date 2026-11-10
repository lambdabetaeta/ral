//! Lexical bindings and the inferring phase of the IR.

use super::solve::{BindHole, CallHole};
use crate::ir::Phase;
use crate::ty::{Scheme, Site, Ty};
use std::collections::{HashMap, HashSet};
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
    recursive: HashSet<String>,
    /// For a binding `let` made of what a call returned, the callee.
    called: HashMap<String, String>,
    handlers: HashMap<String, HandlerBinding>,
}

#[derive(Clone)]
pub(super) struct TyEnv {
    scopes: Vec<NameScope>,
    /// The run's Σ, seeded once by `Solver::seed`.  A name resolves here after a
    /// lexical binding and before a handler.
    pub(crate) builtins: super::builtins::Manifest,
}

impl Default for TyEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl TyEnv {
    pub(super) fn new() -> Self {
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

    pub(crate) fn is_recursive_binding(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .find(|scope| scope.bindings.contains_key(name))
            .is_some_and(|scope| scope.recursive.contains(name))
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

    /// Opened and closed only by `Generator::scope`.
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
        scope.recursive.remove(&name);
        scope.bindings.insert(name, scheme.into());
    }

    pub(crate) fn bind_recursive(&mut self, name: String, scheme: Scheme) {
        self.bind(name.clone(), scheme);
        self.scopes.last_mut().unwrap().recursive.insert(name);
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
}

// ─────────────────────────────────────────────────────────────────────────────
// Inference context
// ─────────────────────────────────────────────────────────────────────────────

/// A tree while its unit is inferred: holes the solution decides, types it
/// solves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Inferring {}

impl Phase for Inferring {
    type Bind = BindHole;
    type Call = CallHole;
    type BindingCall = CallHole;
    type Boundary = Ty;
    type Membrane = Membrane;
}

/// A protected value's contract while inferring.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Membrane {
    /// A checked tree's site, its unit identity included.
    Kept(Arc<Site>),
    /// A session binding's solved type, kept only where it reaches a thunk or
    /// a handle.
    Imported(Ty),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_binding_provenance_respects_shadowing_rebinding_and_scope_exit() {
        let mut env = TyEnv::new();
        env.bind_recursive("f".into(), Scheme::mono(Ty::unit()));
        env.bind("g".into(), Scheme::mono(Ty::unit()));
        assert!(env.is_recursive_binding("f"));
        assert!(!env.is_recursive_binding("g"));
        env.push();
        env.bind("f".into(), Scheme::mono(Ty::unit()));
        assert!(!env.is_recursive_binding("f"));
        env.pop();
        assert!(env.is_recursive_binding("f"));
        env.bind("f".into(), Scheme::mono(Ty::unit()));
        assert!(!env.is_recursive_binding("f"));
    }
}
