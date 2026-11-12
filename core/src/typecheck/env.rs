//! Lexical bindings and the inferring phase of the IR.

use super::solve::{BindHole, CallHole};
use crate::ir::Phase;
use crate::ty::{Scheme, Ty};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Typing environment
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct NameScope {
    bindings: HashMap<String, Arc<Scheme>>,
    recursive: HashSet<String>,
    /// For a binding `let` made of what a call returned, the callee.
    called: HashMap<String, String>,
    handled: HashSet<String>,
}

#[derive(Clone)]
pub(super) struct TyEnv {
    scopes: Vec<NameScope>,
    /// The run's Σ, seeded once by `Solver::seed`.  A name resolves here after a
    /// lexical binding; handlers do not affect typing.
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

    pub(crate) fn is_handled(&self, name: &str) -> bool {
        self.scopes.iter().any(|scope| scope.handled.contains(name))
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
    pub(crate) fn bind_handler(&mut self, name: String) {
        self.scopes.last_mut().unwrap().handled.insert(name);
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
