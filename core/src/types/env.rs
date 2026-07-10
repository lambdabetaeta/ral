//! Lexical environment and process-environment overrides.
//!
//! Two complementary environment stores:
//!
//! - [`Env`] — the lexical scope chain that closures capture.  A
//!   stack of name→value scopes (prelude at bottom, locals pushed
//!   above).  Children receive a fresh clone from the closure's
//!   captured `Arc<Env>`; does not flow through `inherit_from`.
//! - [`EnvVars`] — the `within [shell: …]` dynamic-override map for
//!   process environment variables.  An opaque newtype around
//!   `imbl::HashMap<String, String>`, cheap to clone.  Flows through
//!   `inherit_from` / `spawn_thread` on the [`Context`] subtree.
//!
//! [`Context`]: super::shell::Context

use crate::typecheck::Scheme;
use crate::types::Value;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// One scope entry: the runtime value together with the checker's
/// scheme for the binding, where one survived the turn that installed
/// it.
///
/// Value and scheme are installed together by the statement-level
/// rule (`eval_bind`): a rebind replaces both, a statement that never
/// ran installs neither — there is nothing to keep in sync.
#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub scheme: Option<Scheme>,
}

/// Lexical environment: a stack of name→[`Binding`] scopes, innermost
/// last.
///
/// `Scope[0]` is the prelude, populated by `builtins::register`.  Locals
/// are pushed/popped above it.
///
/// The chain is an `imbl::Vector` rather than a `Vec` so that cloning
/// the env on every closure call (the hot path for recursion) is an
/// O(1) refcount bump on the persistent chunk root, not an
/// O(scope-depth) Arc-bump-per-scope plus heap allocation.  Profile
/// (samply, fold N=10000) showed this clone dominating runtime before
/// the migration.
#[derive(Debug, Clone)]
pub struct Env {
    scopes: imbl::Vector<Arc<HashMap<String, Binding>>>,
}

impl Env {
    /// Fresh environment with one empty scope (no prelude).  The prelude
    /// is loaded by `builtins::register` at shell construction time.
    pub fn new() -> Self {
        Self {
            scopes: imbl::Vector::unit(Arc::new(HashMap::new())),
        }
    }

    /// Look up `name` walking from innermost to outermost scope.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.get_binding(name).map(|b| &b.value)
    }

    /// Look up the [`Binding`] for `name`, walking from innermost to
    /// outermost scope.
    pub fn get_binding(&self, name: &str) -> Option<&Binding> {
        for scope in self.scopes.iter().rev() {
            if let Some(b) = scope.get(name) {
                return Some(b);
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
        for scope in self.scopes.iter().skip(1).rev() {
            if let Some(b) = scope.get(name) {
                return Some(&b.value);
            }
        }
        None
    }

    /// True when the innermost scope is the persisted user scope itself —
    /// `scopes[0]` prelude, `scopes[1]` the user scope `builtins::register`
    /// pushes — with no block/lambda/`if`/`letrec` frame above it.  A binding
    /// installed here survives the turn; anything deeper is popped before it
    /// ends.  Same convention as [`Self::get_local`].
    pub fn at_session_scope(&self) -> bool {
        self.scopes.len() == 2
    }

    /// Look up in the prelude scope only (`scopes[0]`).
    pub fn get_prelude(&self, name: &str) -> Option<&Value> {
        self.scopes
            .front()
            .and_then(|s| s.get(name))
            .map(|b| &b.value)
    }

    /// Bind `name` → `value` in the innermost scope.  A plain set is a
    /// scheme-less install; a rebind through it clears any stored
    /// scheme, since the scheme describes a value the binding no longer
    /// holds.  Routes through [`Self::set_binding`].
    pub fn set(&mut self, name: String, value: Value) {
        self.set_binding(
            name,
            Binding {
                value,
                scheme: None,
            },
        );
    }

    /// The single install point for a scope entry.  Copy-on-write the
    /// top scope's `Arc` so closures that captured this scope are
    /// unaffected.
    pub fn set_binding(&mut self, name: String, binding: Binding) {
        if let Some(scope) = self.scopes.back_mut() {
            Arc::make_mut(scope).insert(name, binding);
        }
    }

    /// Remove `name` from the innermost scope.  Returns the prior
    /// value if it was bound there.  Copy-on-write like [`Self::set`].
    pub fn unset(&mut self, name: &str) -> Option<Value> {
        self.scopes
            .back_mut()
            .and_then(|scope| Arc::make_mut(scope).remove(name))
            .map(|b| b.value)
    }

    /// Push a fresh empty scope.
    pub fn push_scope(&mut self) {
        self.scopes.push_back(Arc::new(HashMap::new()));
    }

    /// Pop the innermost scope.  Refuses to pop the prelude (`scopes[0]`).
    pub fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop_back();
        }
    }

    /// The innermost scope, by reference.  Used by `use` to collect
    /// bindings introduced inside a module body.
    ///
    /// # Panics
    /// Panics if the scope stack is empty, which cannot occur:
    /// [`pop_scope`](Self::pop_scope) always preserves the prelude scope.
    pub fn top_scope(&self) -> &HashMap<String, Binding> {
        self.scopes.back().unwrap()
    }

    /// Walk every scope innermost-first, projecting each binding on first
    /// sight of its name.  The single home of the shadowing rule —
    /// innermost binding wins.
    fn fold_innermost_wins<T>(&self, project: impl Fn(&Binding) -> T) -> Vec<(String, T)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for scope in self.scopes.iter().rev() {
            for (k, b) in scope.as_ref() {
                if seen.insert(k.clone()) {
                    result.push((k.clone(), project(b)));
                }
            }
        }
        result
    }

    /// All bindings across all scopes, innermost wins.
    pub fn all_bindings(&self) -> Vec<(String, Value)> {
        self.fold_innermost_wins(|b| b.value.clone())
    }

    /// Every bound name with its installed scheme, innermost binding
    /// wins.  The seed of the next turn's check: a name with a scheme is
    /// bound to it, a name without one is checked as a bare name.
    pub fn binding_schemes(&self) -> Vec<(String, Option<Scheme>)> {
        self.fold_innermost_wins(|b| b.scheme.clone())
    }

    /// Iterate the scope chain outermost-first, yielding each scope's
    /// `Arc<HashMap>`.  Used by `crate::serial` to intern scopes by
    /// pointer identity; the iterator hides the persistent-vector
    /// backing.
    pub(crate) fn scope_iter(&self) -> impl Iterator<Item = &Arc<HashMap<String, Binding>>> {
        self.scopes.iter()
    }

    /// Build an `Env` from a sequence of scope `Arc`s.  Used by
    /// `crate::serial` when reconstituting a wire-format env on the
    /// receiving side.
    pub(crate) fn from_scope_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = Arc<HashMap<String, Binding>>>,
    {
        Self {
            scopes: iter.into_iter().collect(),
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

// ── EnvVars: process-environment override map ───────────────────────────

/// Persistent string→string map for env-var overrides.
///
/// Cheap to
/// clone; copy-on-write on mutation.  `Serialize` / `Deserialize` are
/// required because [`crate::subprocess::WireContext`] embeds this
/// type and round-trips it as JSON across IPC boundaries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvVars(imbl::HashMap<String, String>);

impl EnvVars {
    /// Empty map.
    pub fn new() -> Self {
        Self(imbl::HashMap::new())
    }

    /// Look up `key`.
    pub fn get(&self, key: &str) -> Option<&String> {
        self.0.get(key)
    }

    /// Look up `key`, preferring this dynamic-override map and
    /// falling back to the host process env.  Single source of truth
    /// for the `within [shell: K=…]` overlay-on-process pattern: any
    /// site that did `env_vars().get(K).cloned().or_else(|| std::env::
    /// var(K).ok())` (or its `unwrap_or_default()` String variant)
    /// belongs here.
    pub fn get_or_host(&self, key: &str) -> Option<String> {
        self.get(key).cloned().or_else(|| std::env::var(key).ok())
    }

    /// Insert `key → value`, returning the prior binding if any.
    pub fn insert(&mut self, key: String, value: String) -> Option<String> {
        self.0.insert(key, value)
    }

    /// Insert `key → value` only if `key` is unbound.  Replaces the
    /// `entry(k).or_insert_with(v)` pattern so the imbl `Entry` type
    /// does not leak.
    pub fn insert_or_keep(&mut self, key: String, value: String) {
        self.0.entry(key).or_insert_with(|| value);
    }

    /// Iterate `(&key, &value)` pairs in arbitrary order.
    pub fn iter(&self) -> EnvVarsIter<'_> {
        EnvVarsIter(self.0.iter())
    }

    /// True when no overrides are present.  An empty map is
    /// observationally equivalent to the bare host environment, so a
    /// caller may skip constructing the overlaid environment (e.g.
    /// in-process uutils).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Borrowing iterator over an [`EnvVars`].
pub struct EnvVarsIter<'a>(
    imbl::hashmap::Iter<'a, String, String, imbl::shared_ptr::DefaultSharedPtr>,
);

impl<'a> Iterator for EnvVarsIter<'a> {
    type Item = (&'a String, &'a String);
    fn next(&mut self) -> Option<(&'a String, &'a String)> {
        self.0.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<'a> IntoIterator for &'a EnvVars {
    type Item = (&'a String, &'a String);
    type IntoIter = EnvVarsIter<'a>;
    fn into_iter(self) -> EnvVarsIter<'a> {
        EnvVarsIter(self.0.iter())
    }
}

impl<K, V> Extend<(K, V)> for EnvVars
where
    K: Into<String>,
    V: Into<String>,
{
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.0.insert(k.into(), v.into());
        }
    }
}

impl<K, V> FromIterator<(K, V)> for EnvVars
where
    K: Into<String>,
    V: Into<String>,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut m = imbl::HashMap::new();
        for (k, v) in iter {
            m.insert(k.into(), v.into());
        }
        Self(m)
    }
}
