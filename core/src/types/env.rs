//! Two environment stores: [`Env`], the lexical scope chain that closures
//! capture and carry themselves, and [`EnvVars`], the `within [env: …]`
//! process-env override map that rides the [`Context`] subtree through
//! `inherit_from` / `spawn_thread`.
//!
//! [`Context`]: super::shell::Context

use crate::typecheck::Scheme;
use crate::types::Value;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// One scope entry.  `eval_bind` installs value and scheme together, so the two
/// never drift apart.
#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub scheme: Option<Scheme>,
}

/// Lexical environment: a stack of name→[`Binding`] scopes, innermost last,
/// with `builtins::register`'s prelude at `scopes[0]` and the user scope it
/// pushes at `scopes[1]`.
///
/// An `imbl::Vector` rather than a `Vec` because every closure call clones the
/// chain: an O(1) refcount bump on the persistent root, not one Arc bump and a
/// heap allocation per scope.  That clone dominated recursion profiles.
#[derive(Debug, Clone)]
pub struct Env {
    scopes: imbl::Vector<Arc<HashMap<String, Binding>>>,
}

impl Env {
    /// Fresh environment with one empty scope — no prelude yet.
    pub fn new() -> Self {
        Self {
            scopes: imbl::Vector::unit(Arc::new(HashMap::new())),
        }
    }

    /// Look up `name`, innermost scope first.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.get_binding(name).map(|b| &b.value)
    }

    /// The whole [`Binding`] for `name`, innermost scope first.
    pub fn get_binding(&self, name: &str) -> Option<&Binding> {
        for scope in self.scopes.iter().rev() {
            if let Some(b) = scope.get(name) {
                return Some(b);
            }
        }
        None
    }

    /// Look up `name` in the local scopes, skipping the prelude.
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

    /// True with no block/lambda/`if`/`letrec` frame above the user scope: a
    /// binding installed here survives the run, anything deeper is popped first.
    pub fn at_session_scope(&self) -> bool {
        self.scopes.len() == 2
    }

    pub fn get_prelude(&self, name: &str) -> Option<&Value> {
        self.get_prelude_binding(name).map(|b| &b.value)
    }

    /// The prelude [`Binding`] for `name`; it carries the checker's harvested
    /// scheme, so a prelude function's type needs no separate registry.
    pub fn get_prelude_binding(&self, name: &str) -> Option<&Binding> {
        self.scopes.front().and_then(|s| s.get(name))
    }

    /// Bind `name` in the innermost scope with no scheme.  A rebind through
    /// here clears any stored scheme, which described a value that is gone.
    pub fn set(&mut self, name: String, value: Value) {
        self.set_binding(
            name,
            Binding {
                value,
                scheme: None,
            },
        );
    }

    /// The single install point for a scope entry.  Copy-on-writes the top
    /// scope so closures that captured it are unaffected.
    pub fn set_binding(&mut self, name: String, binding: Binding) {
        if let Some(scope) = self.scopes.back_mut() {
            Arc::make_mut(scope).insert(name, binding);
        }
    }

    /// Remove `name` from the innermost scope, returning its value.
    pub fn unset(&mut self, name: &str) -> Option<Value> {
        self.scopes
            .back_mut()
            .and_then(|scope| Arc::make_mut(scope).remove(name))
            .map(|b| b.value)
    }

    pub fn push_scope(&mut self) {
        self.scopes.push_back(Arc::new(HashMap::new()));
    }

    /// Pop the innermost scope; refuses to pop the prelude.
    pub fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop_back();
        }
    }

    /// The innermost scope, by reference.  `use` collects a module body's
    /// bindings from here.
    ///
    /// # Panics
    /// Never: [`pop_scope`](Self::pop_scope) always leaves the prelude.
    pub fn top_scope(&self) -> &HashMap<String, Binding> {
        self.scopes.back().unwrap()
    }

    /// Walk every scope innermost-first, projecting each binding on first sight
    /// of its name.  The single home of the shadowing rule.
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

    /// Largest binding's shallow byte estimate, innermost wins, no value cloned.
    pub fn largest_shallow_size(&self) -> usize {
        self.fold_innermost_wins(|b| b.value.shallow_size())
            .into_iter()
            .map(|(_, size)| size)
            .max()
            .unwrap_or(0)
    }

    /// All bindings across all scopes, innermost wins.
    pub fn all_bindings(&self) -> Vec<(String, Value)> {
        self.fold_innermost_wins(|b| b.value.clone())
    }

    /// Distinct bound names across the chain, a shadowed name counted once.
    pub fn distinct_name_count(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for scope in &self.scopes {
            seen.extend(scope.keys().map(String::as_str));
        }
        seen.len()
    }

    /// Every bound name with its scheme, innermost wins.  Seeds the next run's
    /// check: a name without a scheme is checked as a bare name.
    pub fn binding_schemes(&self) -> Vec<(String, Option<Scheme>)> {
        self.fold_innermost_wins(|b| b.scheme.clone())
    }

    /// Iterate the chain outermost-first.  `crate::serial` interns scopes by
    /// pointer identity, so it needs the `Arc`s, not their contents.
    pub(crate) fn scope_iter(&self) -> impl Iterator<Item = &Arc<HashMap<String, Binding>>> {
        self.scopes.iter()
    }

    /// Rebuild an `Env` from scope `Arc`s — `crate::serial`'s receiving side.
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

/// Persistent string→string map of env-var overrides, cheap to clone.
///
/// `Serialize` / `Deserialize` are required because
/// [`crate::subprocess::WireContext`] embeds this type and round-trips it as
/// JSON across IPC boundaries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvVars(imbl::HashMap<String, String>);

impl EnvVars {
    pub fn new() -> Self {
        Self(imbl::HashMap::new())
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.0.get(key)
    }

    /// Look up `key`, this override map first, then the host process env.  The
    /// one home of that fallback: no caller should spell it out again.
    pub fn get_or_host(&self, key: &str) -> Option<String> {
        self.get(key).cloned().or_else(|| std::env::var(key).ok())
    }

    pub fn insert(&mut self, key: String, value: String) -> Option<String> {
        self.0.insert(key, value)
    }

    /// Insert only if `key` is unbound, without leaking imbl's `Entry` type.
    pub fn insert_or_keep(&mut self, key: String, value: String) {
        self.0.entry(key).or_insert_with(|| value);
    }

    pub fn iter(&self) -> EnvVarsIter<'_> {
        EnvVarsIter(self.0.iter())
    }

    /// True when the bare host environment will do and a caller may skip
    /// building the overlay — the in-process uutils path turns on this.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

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
