//! Two environment stores: [`Env`], the finite map a closure captures and
//! carries itself, and [`EnvVars`], the `within [env: …]` process-env
//! override map that rides the [`Context`] subtree through `inherit_from` /
//! `spawn_thread`.
//!
//! [`Context`]: super::shell::Context

use crate::typecheck::Scheme;
use crate::types::Value;
use rustc_hash::FxBuildHasher;
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

/// Every key hashed below is a program identifier, never attacker-controlled
/// input, so the three tiers use a fast non-cryptographic hasher.
pub(crate) type NativeMap = HashMap<String, Value, FxBuildHasher>;
pub(crate) type PreludeMap = HashMap<String, Binding, FxBuildHasher>;
pub(crate) type BindingMap =
    imbl::GenericHashMap<String, Binding, FxBuildHasher, imbl::shared_ptr::DefaultSharedPtr>;

/// Lexical environment: three tiers, checked in order.
///
/// `natives` are language constants — seeded once at boot
/// ([`Self::install_natives`]), never written after. `prelude` is the baked
/// prelude's bindings, one map per process, shared by every shell that boots
/// from it. `bindings` is everything bound since: a persistent map, so
/// `bind` is O(log₃₂ n) and cloning the whole environment is O(1) — the
/// clone every closure capture and every save/restore bracket takes.
#[derive(Debug, Clone)]
pub struct Env {
    natives: Arc<NativeMap>,
    prelude: Arc<PreludeMap>,
    bindings: BindingMap,
}

impl Env {
    /// The empty three-tier map: no natives, no prelude, nothing bound.
    pub fn new() -> Self {
        Self {
            natives: Arc::new(NativeMap::default()),
            prelude: Arc::new(PreludeMap::default()),
            bindings: BindingMap::default(),
        }
    }

    /// `natives` alone, no prelude — what a prelude bake itself runs under.
    pub(crate) fn with_natives(natives: Arc<NativeMap>) -> Self {
        Self {
            natives,
            prelude: Arc::new(PreludeMap::default()),
            bindings: BindingMap::default(),
        }
    }

    /// Seat a shell: `natives` and the baked `prelude`, nothing bound yet.
    pub(crate) fn with_prelude(natives: Arc<NativeMap>, prelude: Arc<PreludeMap>) -> Self {
        Self {
            natives,
            prelude,
            bindings: BindingMap::default(),
        }
    }

    /// Rebuild an `Env` from its three tiers — `crate::serial`'s receiving
    /// side, which decodes only `bindings` and seats it under the receiver's
    /// own `natives`/`prelude`.
    pub(crate) fn from_parts(
        natives: Arc<NativeMap>,
        prelude: Arc<PreludeMap>,
        bindings: BindingMap,
    ) -> Self {
        Self {
            natives,
            prelude,
            bindings,
        }
    }

    /// Seed the base native scope — a value manifest row's `Value`, or a
    /// language-given constant.  Called only at boot, beside builtin-table
    /// installation.
    pub(crate) fn install_natives(&mut self, entries: impl IntoIterator<Item = (String, Value)>) {
        let map = Arc::make_mut(&mut self.natives);
        map.extend(entries);
    }

    /// Look up `name`: `bindings`, then `prelude`, then `natives`.
    pub fn get(&self, name: &str) -> Option<&Value> {
        if let Some(b) = self.get_binding(name) {
            return Some(&b.value);
        }
        self.natives.get(name)
    }

    /// The whole [`Binding`] for `name`: `bindings`, then `prelude`.  Natives
    /// carry no [`Binding`] — no scheme, no source location — so a native-only
    /// hit answers [`None`] here even though [`Self::get`] resolves it.
    pub fn get_binding(&self, name: &str) -> Option<&Binding> {
        self.bindings.get(name).or_else(|| self.prelude.get(name))
    }

    /// Every native's name — a host's tab completion or listing surface, the
    /// native-scope counterpart of [`crate::types::BuiltinTable::names`].
    pub fn native_names(&self) -> impl Iterator<Item = &str> {
        self.natives.keys().map(String::as_str)
    }

    /// The prelude [`Binding`] for `name`; it carries the checker's harvested
    /// scheme, so a prelude function's type needs no separate registry.
    pub fn prelude_binding(&self, name: &str) -> Option<&Binding> {
        self.prelude.get(name)
    }

    /// The session [`Binding`] for `name` — everything bound since the
    /// prelude, skipping it.  What `help`'s local-site lookups want.
    pub fn session_binding(&self, name: &str) -> Option<&Binding> {
        self.bindings.get(name)
    }

    /// Every name bound since the prelude — what the binding lease adopts.
    pub fn session_names(&self) -> impl Iterator<Item = &str> {
        self.bindings.keys().map(String::as_str)
    }

    /// Bind `name` in the session tier, replacing any existing binding —
    /// persistent, so an environment a closure already captured is
    /// unaffected.  A rebind through here clears any stored scheme, which
    /// described a value that is gone.
    pub fn bind(&mut self, name: String, binding: Binding) {
        self.bindings.insert(name, binding);
    }

    /// Remove `name` from the session tier, returning its value; a prelude
    /// name of the same spelling reappears beneath.
    pub fn unset(&mut self, name: &str) -> Option<Value> {
        self.bindings
            .remove(name)
            .map(|mut b| std::mem::replace(&mut b.value, Value::Unit))
    }

    /// The serializable fragment of this environment: every `Value::Handle`
    /// binding in the session tier replaced by its opaque placeholder.  The
    /// prelude is untouched — it is baked before any handle exists, so no
    /// handle ever reaches it — and neither is `natives`, seeded once at
    /// boot from language constants.
    ///
    /// The one snapshot law's whole mechanism: an identity fork and a wire
    /// seed must resolve every name to the same value or the same absence,
    /// and a handle has no wire form, so both arms scrub it the same way —
    /// this one, called from the one place both pass through,
    /// `Shell::fork_scrubbed`.
    pub(crate) fn scrub_handles(&self) -> Self {
        let bindings = self
            .bindings
            .iter()
            .map(|(name, binding)| {
                let value = crate::serial::scrub(&binding.value, &crate::serial::is_handle);
                (
                    name.clone(),
                    Binding {
                        value,
                        scheme: binding.scheme.clone(),
                    },
                )
            })
            .collect();
        Self {
            natives: self.natives.clone(),
            prelude: self.prelude.clone(),
            bindings,
        }
    }

    /// Walk `bindings` then `prelude`, projecting each binding on first sight
    /// of its name.  The single home of the shadowing rule.
    fn fold_union<T>(&self, project: impl Fn(&Binding) -> T) -> Vec<(String, T)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::with_capacity(self.bindings.len() + self.prelude.len());
        for (k, b) in &self.bindings {
            seen.insert(k.as_str());
            result.push((k.clone(), project(b)));
        }
        for (k, b) in self.prelude.iter() {
            if !seen.contains(k.as_str()) {
                result.push((k.clone(), project(b)));
            }
        }
        result
    }

    /// Largest binding's shallow byte estimate, session wins, no value cloned.
    pub fn largest_shallow_size(&self) -> usize {
        self.fold_union(|b| b.value.shallow_size())
            .into_iter()
            .map(|(_, size)| size)
            .max()
            .unwrap_or(0)
    }

    /// Every binding across prelude and session, session wins.
    pub fn all_bindings(&self) -> Vec<(String, Value)> {
        self.fold_union(|b| b.value.clone())
    }

    /// Distinct bound names across prelude and session, a shadowed name
    /// counted once.
    pub fn distinct_name_count(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        seen.extend(self.bindings.keys().map(String::as_str));
        seen.extend(self.prelude.keys().map(String::as_str));
        seen.len()
    }

    /// Every bound name with its scheme, session wins.  Seeds the next run's
    /// check: a name without a scheme is checked as a bare name.
    pub fn binding_schemes(&self) -> Vec<(String, Option<Scheme>)> {
        self.fold_union(|b| b.scheme.clone())
    }

    /// The session tier's persistent map root — `crate::serial` interns
    /// environments by this root's identity, and needs the map itself, not
    /// its contents, to compare by [`imbl::GenericHashMap::ptr_eq`].
    pub(crate) fn bindings_root(&self) -> &BindingMap {
        &self.bindings
    }

    /// This environment's native tier, for a decoder seating a hydrated
    /// environment under the receiver's own — natives never ride the wire.
    pub(crate) fn natives_arc(&self) -> Arc<NativeMap> {
        Arc::clone(&self.natives)
    }

    /// This environment's prelude tier, for a decoder seating a hydrated
    /// environment under the receiver's own — the prelude never rides the
    /// wire either.
    pub(crate) fn prelude_arc(&self) -> Arc<PreludeMap> {
        Arc::clone(&self.prelude)
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
    use super::*;

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

    /// A name a `let` shadows over a prelude binding of the same spelling
    /// round-trips as the user's value, and `unset` reveals the prelude's
    /// underneath — the same behaviour the old scope stack gave, now from a
    /// flat map with no scope to pop.
    #[test]
    fn shadowed_prelude_name_round_trips_then_unset_reveals_it() {
        let mut prelude = PreludeMap::default();
        prelude.insert(
            "map".to_string(),
            Binding {
                value: Value::String("prelude-map".into()),
                scheme: None,
            },
        );
        let mut env = Env::with_prelude(Arc::new(NativeMap::default()), Arc::new(prelude));
        assert_eq!(env.get("map"), Some(&Value::String("prelude-map".into())));

        env.bind(
            "map".to_string(),
            Binding {
                value: Value::Int(3),
                scheme: None,
            },
        );
        assert_eq!(env.get("map"), Some(&Value::Int(3)));

        env.unset("map");
        assert_eq!(env.get("map"), Some(&Value::String("prelude-map".into())));
    }
}
