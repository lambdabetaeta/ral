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

/// Lexical environment: a stack of name→[`Binding`] scopes, innermost last.
///
/// `builtins::register`'s prelude sits at `scopes[0]` and the user scope it
/// pushes at `scopes[1]`, with the base native scope beneath them all —
/// seeded once at boot ([`Self::install_natives`]), reached by [`Self::get`]
/// only after every scope misses, outside every scope harvest, and
/// removable by nothing: no method here unsets from it.
///
/// An `imbl::Vector` rather than a `Vec` because every closure call clones the
/// chain: an O(1) refcount bump on the persistent root, not one Arc bump and a
/// heap allocation per scope.  That clone dominated recursion profiles.
#[derive(Debug, Clone)]
pub struct Env {
    scopes: imbl::Vector<Arc<HashMap<String, Binding>>>,
    natives: Arc<HashMap<String, Value>>,
}

impl Env {
    /// Fresh environment with one empty scope — no prelude yet.
    pub fn new() -> Self {
        Self {
            scopes: imbl::Vector::unit(Arc::new(HashMap::new())),
            natives: Arc::new(HashMap::new()),
        }
    }

    /// Seed the base native scope — a value manifest row's `Value`, or a
    /// language-given constant.  Called only at boot, beside builtin-table
    /// installation.
    pub(crate) fn install_natives(&mut self, entries: impl IntoIterator<Item = (String, Value)>) {
        let map = Arc::make_mut(&mut self.natives);
        map.extend(entries);
    }

    /// Look up `name`, innermost scope first, falling back to the base
    /// native scope.
    pub fn get(&self, name: &str) -> Option<&Value> {
        if let Some(b) = self.get_binding(name) {
            return Some(&b.value);
        }
        self.natives.get(name)
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

    /// Every native's name — a host's tab completion or listing surface, the
    /// native-scope counterpart of [`crate::types::BuiltinTable::names`].
    pub fn native_names(&self) -> impl Iterator<Item = &str> {
        self.natives.keys().map(String::as_str)
    }

    /// Look up `name` in the local scopes, skipping the prelude.
    pub fn get_local(&self, name: &str) -> Option<&Value> {
        self.get_local_binding(name).map(|b| &b.value)
    }

    /// The whole [`Binding`] for `name` in the local scopes, skipping the
    /// prelude; a checked top-level `let` carries its generalised scheme here.
    pub fn get_local_binding(&self, name: &str) -> Option<&Binding> {
        if self.scopes.len() < 2 {
            return None;
        }
        self.scopes
            .iter()
            .skip(1)
            .rev()
            .find_map(|scope| scope.get(name))
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
            .map(|mut b| std::mem::replace(&mut b.value, Value::Unit))
    }

    /// The serializable fragment of this scope: every `Value::Handle`
    /// binding, in every scope of the chain, replaced by its opaque
    /// placeholder. The natives scope is untouched — it is seeded once at
    /// boot from language constants, never from a running session, so no
    /// handle ever reaches it.
    ///
    /// The one snapshot law's whole mechanism: an identity fork and a wire
    /// seed must resolve every name to the same value or the same absence,
    /// and a handle has no wire form, so both arms scrub it the same way —
    /// this one, called from the one place both pass through,
    /// `Shell::fork_scrubbed`.
    pub(crate) fn scrub_handles(&self) -> Self {
        let scopes = self
            .scopes
            .iter()
            .map(|scope| {
                Arc::new(
                    scope
                        .iter()
                        .map(|(name, binding)| {
                            let value =
                                crate::serial::scrub(&binding.value, &crate::serial::is_handle);
                            (
                                name.clone(),
                                Binding {
                                    value,
                                    scheme: binding.scheme.clone(),
                                },
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        Self {
            scopes,
            natives: self.natives.clone(),
        }
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
    ///
    /// `natives` is the receiving manifest's own native map, not the
    /// sender's: natives cross the wire by name only, so a hydrated
    /// closure's base scope must be rebuilt here or its bare references
    /// resolve to nothing.
    pub(crate) fn from_scope_iter<I>(iter: I, natives: Arc<HashMap<String, Value>>) -> Self
    where
        I: IntoIterator<Item = Arc<HashMap<String, Binding>>>,
    {
        Self {
            scopes: iter.into_iter().collect(),
            natives,
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// `Some` while a dismantling [`Binding::drop`] is looping on this thread.
    /// A binding drop reached from inside that loop pushes its value here
    /// instead of letting drop glue recurse into it.
    static DISMANTLE_QUEUE: std::cell::RefCell<Option<Vec<Value>>> =
        const { std::cell::RefCell::new(None) };
}

impl Drop for Binding {
    /// A stream is a chain of closures — block → captured env → scope →
    /// binding → block — so plain drop glue recurses once per link and bounds
    /// stream length by the stack.  Every link passes through a `Binding`, so
    /// the chain is cut here, with a trampoline rather than a hand-rolled
    /// walk: glue still does all traversal — a shared spine stays one
    /// refcount decrement, nothing is cloned to be destroyed — but a binding
    /// dying *inside* another binding's drop hands its value to that
    /// dismantler's queue and returns, keeping the stack between any two
    /// links constant.
    fn drop(&mut self) {
        // A value with no interior can reach no binding: let glue have it.
        if matches!(
            self.value,
            Value::Unit
                | Value::Bool(_)
                | Value::Int(_)
                | Value::Float(_)
                | Value::String(_)
                | Value::Bytes(_)
                | Value::Variant { payload: None, .. }
        ) {
            return;
        }
        let value = std::mem::replace(&mut self.value, Value::Unit);
        // Hand the value to a dismantler above us, or become one.  If the
        // queue is already torn down (a binding dying during thread-local
        // destruction), the unrun closure drops `value` — glue alone, the
        // honest fallback.
        let Ok(value) = DISMANTLE_QUEUE.try_with(|slot| {
            let mut q = slot.borrow_mut();
            if let Some(queue) = q.as_mut() {
                queue.push(value);
                None
            } else {
                *q = Some(Vec::new());
                Some(value)
            }
        }) else {
            return;
        };
        // Enqueued: the dismantler above owns it now.
        let Some(value) = value else { return };
        /// Disarms the queue even on unwind; leftovers then elect fresh
        /// leaders of their own.
        struct Disarm;
        impl Drop for Disarm {
            fn drop(&mut self) {
                let _ = DISMANTLE_QUEUE.try_with(|slot| slot.borrow_mut().take());
            }
        }
        let _disarm = Disarm;
        let mut next = Some(value);
        while let Some(v) = next {
            drop(v);
            next = DISMANTLE_QUEUE.with(|slot| slot.borrow_mut().as_mut().and_then(Vec::pop));
        }
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

#[cfg(test)]
mod tests {
    /// (Regression: drop glue recursed once per link, and a sixty-thousand
    /// line stream aborted the process at teardown.)
    #[test]
    fn deep_closure_chain_drops_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| drop(crate::types::deep_block_chain(100_000)))
            .expect("spawn")
            .join()
            .expect("a deep chain must drop without exhausting the stack");
    }
}
