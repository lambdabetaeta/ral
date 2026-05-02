//! Lexical environment (ρ).
//!
//! The variable→value bindings closures capture and that scope rules
//! follow.  In the flow matrix this row is "from closure" on every
//! child boundary — `Env` does not flow through `inherit_from`,
//! `spawn_thread`, or sandbox IPC like the other components.  Children
//! receive a fresh `Env` cloned from the closure's captured `Arc<Env>`.
//!
//! Scopes are `Arc<HashMap<...>>` directly.  `Arc` (rather than `Rc`)
//! is required so that captured envs can cross thread boundaries via
//! `_fork`/`spawn_thread`; naming it `Arc` makes the Send-safety
//! visible at the type level.  `Value::Thunk { captured: Arc<Env> }`
//! lifts the same trick one level up: cloning a thunk is one refcount
//! bump on the outer `Arc<Env>`, not a `Vec`-clone of the scope chain.
//!
//! Wire-format serialisation (across the sandbox process boundary) is
//! handled in `crate::serial` — there `Env` is interned scope-by-scope
//! into a flat `Vec<u32>`-indexed table to dedupe shared scopes, and
//! reconstituted on the receiving side.  `Env` itself stays in-process.

use crate::types::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Lexical environment: a stack of name→value scopes, innermost last.
///
/// Scope[0] is the prelude, populated by `builtins::register`.  Locals
/// are pushed/popped above it.
#[derive(Debug, Clone, PartialEq)]
pub struct Env {
    scopes: Vec<Arc<HashMap<String, Value>>>,
}

impl Env {
    /// Fresh environment with one empty scope (no prelude).  The prelude
    /// is loaded by `builtins::register` at shell construction time.
    pub fn new() -> Self {
        Self {
            scopes: vec![Arc::new(HashMap::new())],
        }
    }

    /// Look up `name` walking from innermost to outermost scope.
    pub fn get(&self, name: &str) -> Option<&Value> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v);
            }
        }
        None
    }

    /// Look up in local scopes only (`scopes[1..]`, skipping the
    /// prelude at `scopes[0]`).
    pub fn get_local(&self, name: &str) -> Option<&Value> {
        if self.scopes.len() < 2 {
            return None;
        }
        for scope in self.scopes[1..].iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v);
            }
        }
        None
    }

    /// Look up in the prelude scope only (`scopes[0]`).
    pub fn get_prelude(&self, name: &str) -> Option<&Value> {
        self.scopes.first().and_then(|s| s.get(name))
    }

    /// Bind `name` → `value` in the innermost scope.  Copy-on-write the
    /// top scope's `Arc` so closures that captured this scope are
    /// unaffected.
    pub fn set(&mut self, name: String, value: Value) {
        if let Some(scope) = self.scopes.last_mut() {
            Arc::make_mut(scope).insert(name, value);
        }
    }

    /// Push a fresh empty scope.
    pub fn push_scope(&mut self) {
        self.scopes.push(Arc::new(HashMap::new()));
    }

    /// Pop the innermost scope.  Refuses to pop the prelude (`scopes[0]`).
    pub fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    /// The innermost scope, by reference.  Used by `use` to collect
    /// bindings introduced inside a module body.
    pub fn top_scope(&self) -> &HashMap<String, Value> {
        self.scopes.last().unwrap()
    }

    /// All bindings across all scopes, innermost wins.
    pub fn all_bindings(&self) -> Vec<(String, Value)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for scope in self.scopes.iter().rev() {
            for (k, v) in scope.as_ref() {
                if seen.insert(k.clone()) {
                    result.push((k.clone(), v.clone()));
                }
            }
        }
        result
    }

    /// Borrow the underlying scope chain.  Used by `crate::serial` to
    /// intern scopes by `Arc` pointer identity.
    #[cfg(unix)]
    pub(crate) fn scopes(&self) -> &[Arc<HashMap<String, Value>>] {
        &self.scopes
    }

    /// Build an `Env` from a raw scope vector.  Used by `crate::serial`
    /// when reconstituting a wire-format env on the receiving side.
    #[cfg(unix)]
    pub(crate) fn from_scopes(scopes: Vec<Arc<HashMap<String, Value>>>) -> Self {
        Self { scopes }
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}
