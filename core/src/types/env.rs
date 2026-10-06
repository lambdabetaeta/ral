//! Two environment stores: [`Env`], the finite map a closure captures and
//! carries itself, and [`EnvVars`], the `within [env: …]` process-env
//! override map that rides the [`Context`] subtree into every child shell.
//!
//! [`Context`]: super::shell::Context

use crate::ir::{Name, Occ};
use crate::ty::Scheme;
use crate::types::Value;
use crate::types::signature::Signature;
use either::Either;
use rustc_hash::FxBuildHasher;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};

/// One scope entry: value and scheme are installed together, so the two
/// never drift apart.
#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    /// Shared, not owned: a scheme dwarfs the value it describes, and every
    /// `bind` into a shared environment copies a whole node of these.
    pub(crate) scheme: Option<Arc<Scheme>>,
}

/// Past [`SMALL`] entries, ρ is a persistent hash map; every key here is a
/// program identifier, never attacker-controlled input, so it uses a fast
/// non-cryptographic hasher.
type LargeMap =
    imbl::GenericHashMap<Name, Binding, FxBuildHasher, imbl::shared_ptr::DefaultSharedPtr>;

/// Up to this many entries ρ is one flat array; past it, a persistent hash
/// map.  8 beat 16 and 32 on B6 and tied on B1 and B2.
const SMALL: usize = 8;

/// ρ's two representations, chosen by size and invisible to `Env`'s users.
#[derive(Debug, Clone)]
enum Entries {
    /// Sorted by name, distinct, one allocation.
    Small(Arc<[(Name, Binding)]>),
    Large(Arc<LargeMap>),
}

/// One static empty `Small`, so a value capturing nothing allocates nothing.
static EMPTY: LazyLock<Arc<[(Name, Binding)]>> = LazyLock::new(|| Arc::from(Vec::new()));

/// `entries` as `Entries`, sharing the one static [`EMPTY`] when there are
/// none — every path that could otherwise build a fresh empty `Small`
/// (`extend`, `unset`, `restrict`) goes through here instead.
fn small_or_large(entries: Vec<(Name, Binding)>) -> Entries {
    if entries.is_empty() {
        Entries::Small(Arc::clone(&EMPTY))
    } else if entries.len() > SMALL {
        Entries::Large(Arc::new(entries.into_iter().collect()))
    } else {
        Entries::Small(entries.into())
    }
}

/// ρ: a persistent map from names to bindings, read alone.
///
/// No Σ fallback — [`crate::types::signature::lookup`] is the resolution rule
/// that adds Σ. `restrict` is where a thunk value's capture is scrubbed to
/// what it mentions; `bind`/`extend` are the write side, each producing a
/// fresh environment so one a closure already captured is untouched.
#[derive(Debug, Clone)]
pub struct Env(Entries);

const _: () = assert!(std::mem::size_of::<Env>() <= 16, "Env must fit in 16 bytes");

impl Env {
    /// The empty map — one static allocation, cloned.
    pub fn new() -> Self {
        Self(Entries::Small(Arc::clone(&EMPTY)))
    }

    pub(crate) fn len(&self) -> usize {
        match &self.0 {
            Entries::Small(a) => a.len(),
            Entries::Large(m) => m.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up `name`: ρ alone, no Σ fallback.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.binding(name).map(|b| &b.value)
    }

    /// The whole [`Binding`] for `name`, ρ alone.
    pub(crate) fn binding(&self, name: &str) -> Option<&Binding> {
        match &self.0 {
            Entries::Small(a) => a
                .binary_search_by(|(n, _)| n.as_ref().cmp(name))
                .ok()
                .map(|i| &a[i].1),
            Entries::Large(m) => m.get(name),
        }
    }

    /// Bind `name` in ρ, replacing any existing binding — persistent, so an
    /// environment a closure already captured is unaffected. The hot path,
    /// every `let`: a `Small` below the cap builds its one fresh array
    /// directly from the unaffected slices either side of the insertion
    /// point, writing a `TrustedLen` iterator in place with no intermediate
    /// `Vec`.
    pub(crate) fn bind(&mut self, name: Name, binding: Binding) {
        if let Entries::Small(a) = &self.0 {
            let (i, j) = match a.binary_search_by(|(n, _)| n.as_ref().cmp(name.as_ref())) {
                Ok(i) => (i, i + 1),
                Err(i) => (i, i),
            };
            if a.len() - (j - i) < SMALL {
                self.0 = Entries::Small(
                    a[..i]
                        .iter()
                        .cloned()
                        .chain(std::iter::once((name, binding)))
                        .chain(a[j..].iter().cloned())
                        .collect(),
                );
                return;
            }
        }
        self.extend(std::iter::once((name, binding)));
    }

    /// Bind every entry in one pass — a pattern's names, or a `Rec` group's
    /// members — a later entry shadowing an earlier one, in this call or
    /// already in `self`. `bind` is the one-entry fast path; this is the
    /// general merge, past `SMALL` a unique `Large` updated in place.
    pub(crate) fn extend(&mut self, entries: impl ExactSizeIterator<Item = (Name, Binding)>) {
        if entries.len() == 0 {
            return;
        }
        match &mut self.0 {
            Entries::Small(a) => {
                let mut merged: Vec<(Name, Binding)> = Vec::with_capacity(a.len() + entries.len());
                merged.extend(a.iter().cloned());
                for (name, binding) in entries {
                    match merged.binary_search_by(|(n, _)| n.as_ref().cmp(name.as_ref())) {
                        Ok(i) => merged[i] = (name, binding),
                        Err(i) => merged.insert(i, (name, binding)),
                    }
                }
                self.0 = small_or_large(merged);
            }
            Entries::Large(m) => {
                let m = Arc::make_mut(m);
                for (name, binding) in entries {
                    m.insert(name, binding);
                }
            }
        }
    }

    /// Remove `name` from ρ, returning its value.
    pub(crate) fn unset(&mut self, name: &str) -> Option<Value> {
        match &mut self.0 {
            Entries::Small(a) => {
                let i = a.binary_search_by(|(n, _)| n.as_ref().cmp(name)).ok()?;
                let mut kept: Vec<(Name, Binding)> = a.iter().cloned().collect();
                let (_, binding) = kept.remove(i);
                self.0 = small_or_large(kept);
                Some(binding.value)
            }
            Entries::Large(m) => {
                let map = Arc::make_mut(m);
                let binding = map.remove(name)?;
                Some(binding.value)
            }
        }
    }

    /// `self` narrowed to `occ`. Tested identity first: if `occ` covers every
    /// name `self` binds, `self` is returned unchanged, the same allocation —
    /// sound because an [`Occ`] is distinct by construction. Otherwise built
    /// by walking `occ`, not `self`: a `Small` of the hits, or a `Large` past
    /// [`SMALL`].
    pub(crate) fn restrict(&self, occ: &Occ) -> Self {
        if occ.len() >= self.len() && self.names().all(|name| occ.contains(name)) {
            return self.clone();
        }
        let hits: Vec<(Name, Binding)> = occ
            .names()
            .filter_map(|n| self.binding(n).map(|b| (n.clone(), b.clone())))
            .collect();
        // `occ` is sorted and distinct, so `hits` stays sorted: `Small`'s invariant holds with no further sort.
        Self(small_or_large(hits))
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &Name> {
        self.iter().map(|(name, _)| name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Name, &Binding)> {
        match &self.0 {
            Entries::Small(a) => Either::Left(a.iter().map(|(name, binding)| (name, binding))),
            Entries::Large(m) => Either::Right(m.iter()),
        }
    }

    /// The wire's and the fork scrub's interning key: two environments share
    /// this identity exactly when they are the same allocation — the same
    /// `Small` array or the same `Large` map.
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Entries::Small(a), Entries::Small(b)) => Arc::ptr_eq(a, b),
            (Entries::Large(a), Entries::Large(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Walk ρ, then Σ's prelude, projecting each binding on first sight of
    /// its name.  The single home of the shadowing rule.
    pub(crate) fn fold_union<T>(
        &self,
        sig: &Signature,
        project: impl Fn(&Binding) -> T,
    ) -> Vec<(String, T)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::with_capacity(self.len());
        for (k, b) in self.iter() {
            seen.insert(k.as_ref());
            result.push((k.to_string(), project(b)));
        }
        for name in sig.prelude_names() {
            if !seen.contains(name)
                && let Some(b) = sig.prelude_binding(name)
            {
                result.push((name.to_string(), project(b)));
            }
        }
        result
    }

    /// Largest binding's shallow byte estimate, ρ then Σ's prelude, no value
    /// cloned.
    pub(crate) fn largest_shallow_size(&self, sig: &Signature) -> usize {
        let session = self.iter().map(|(_, b)| b.value.shallow_size());
        let shadowed_prelude = sig
            .prelude_names()
            .filter(|name| self.binding(name).is_none())
            .filter_map(|name| sig.prelude_binding(name))
            .map(|b| b.value.shallow_size());
        session.chain(shadowed_prelude).max().unwrap_or(0)
    }

    /// Every binding across ρ and Σ's prelude, ρ wins.
    pub(crate) fn all_bindings(&self, sig: &Signature) -> Vec<(String, Value)> {
        self.fold_union(sig, |b| b.value.clone())
    }

    /// Distinct bound names across ρ and Σ's prelude, a shadowed name counted
    /// once.
    pub(crate) fn distinct_name_count(&self, sig: &Signature) -> usize {
        let mut seen: std::collections::HashSet<&str> = self.names().map(AsRef::as_ref).collect();
        seen.extend(sig.prelude_names());
        seen.len()
    }

    /// Every bound name with its scheme, ρ then Σ's prelude, ρ wins.  Seeds
    /// the next run's check: a name without a scheme is checked as a bare
    /// name.
    pub(crate) fn binding_schemes(&self, sig: &Signature) -> Vec<(String, Option<Arc<Scheme>>)> {
        self.fold_union(sig, |b| b.scheme.clone())
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// `Some` while a dismantling [`Env::dismantle`] is looping on this
    /// thread.  A closure dying inside that loop pushes its entries here
    /// instead of letting drop glue recurse into them.
    static DISMANTLE_QUEUE: std::cell::RefCell<Option<Vec<Entries>>> =
        const { std::cell::RefCell::new(None) };
}

impl Env {
    /// A stream is a chain of closures — block → captured env → binding →
    /// block — so plain drop glue recurses once per link and bounds stream
    /// length by the stack.  Every link passes through a closure, so
    /// [`Closure`](super::Closure)'s drop cuts the chain here, with a
    /// trampoline rather than a hand-rolled walk: glue still does all
    /// traversal — a shared spine stays one refcount decrement, nothing is
    /// cloned to be destroyed — but a closure dying *inside* another's
    /// dismantling hands its entries to that dismantler's queue and returns,
    /// keeping the stack between any two links constant.
    pub(crate) fn dismantle(&mut self) {
        if self.is_empty() {
            return;
        }
        let entries = std::mem::replace(&mut self.0, Entries::Small(Arc::clone(&EMPTY)));
        // Hand the entries to a dismantler above us, or become one.  If the
        // queue is already torn down (a closure dying during thread-local
        // destruction), the unrun closure drops `entries` — glue alone, the
        // honest fallback.
        let Ok(entries) = DISMANTLE_QUEUE.try_with(|slot| {
            let mut q = slot.borrow_mut();
            if let Some(queue) = q.as_mut() {
                queue.push(entries);
                None
            } else {
                *q = Some(Vec::new());
                Some(entries)
            }
        }) else {
            return;
        };
        // Enqueued: the dismantler above owns it now.
        let Some(entries) = entries else { return };
        /// Disarms the queue even on unwind; leftovers then elect fresh
        /// leaders of their own.
        struct Disarm;
        impl Drop for Disarm {
            fn drop(&mut self) {
                let _ = DISMANTLE_QUEUE.try_with(|slot| slot.borrow_mut().take());
            }
        }
        let _disarm = Disarm;
        let mut next = Some(entries);
        while let Some(e) = next {
            drop(e);
            next = DISMANTLE_QUEUE.with(|slot| slot.borrow_mut().as_mut().and_then(Vec::pop));
        }
    }
}

/// Persistent string→string map of env-var overrides, cheap to clone.
///
/// `Serialize` / `Deserialize` are required because
/// the seed's [`Context`](crate::types::Context) embeds this type and
/// round-trips it as JSON across IPC boundaries.
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
    /// one home of that fallback: no caller should spell it out again.  A host
    /// value that is not UTF-8 reads as unbound, as in [`Self::host_text`].
    pub(crate) fn get_or_host(&self, key: &str) -> Option<String> {
        self.get(key).cloned().or_else(|| std::env::var(key).ok())
    }

    /// `HOME`, then `USERPROFILE`: each key overlay first, then the host.
    pub fn home(&self) -> Option<String> {
        crate::host::first_bound(&crate::host::HOME_VARS, |k| self.get_or_host(k))
    }

    /// `USER`, then `USERNAME`, as [`Self::home`].
    pub fn user(&self) -> Option<String> {
        crate::host::first_bound(&crate::host::USER_VARS, |k| self.get_or_host(k))
    }

    /// The host process env as ral reads it: text only.  A pair that is not
    /// UTF-8 is left out, never mangled; children still inherit its bytes.
    pub(crate) fn host_text() -> impl Iterator<Item = (String, String)> {
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
    }

    pub(crate) fn insert(&mut self, key: String, value: String) -> Option<String> {
        self.0.insert(key, value)
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
    use crate::types::signature::Signature;

    fn binding(value: Value) -> Binding {
        Binding {
            value,
            scheme: None,
        }
    }

    fn env_of<const N: usize>(pairs: [(&str, i64); N]) -> Env {
        let mut env = Env::new();
        for (name, n) in pairs {
            env.bind(name.into(), binding(Value::Int(n)));
        }
        env
    }

    /// (Regression: drop glue recursed once per link, and a sixty-thousand-link
    /// lazy list aborted the process at teardown.)
    #[test]
    fn deep_closure_chain_drops_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| drop(crate::types::deep_block_chain(100_000, Value::Unit)))
            .expect("spawn")
            .join()
            .expect("a deep chain must drop without exhausting the stack");
    }

    /// A name a `let` shadows over a prelude binding of the same spelling
    /// round-trips as the user's value, and `unset` reveals the prelude's
    /// underneath through `lookup`.
    #[test]
    fn shadowed_prelude_name_round_trips_then_unset_reveals_it() {
        let mut sig = Signature::default();
        sig.install_prelude(Arc::new(
            std::iter::once(("map".to_string(), binding(Value::string("prelude-map")))).collect(),
        ));
        let mut env = Env::new();
        assert_eq!(
            crate::types::signature::lookup("map", &env, &sig),
            Some(&Value::string("prelude-map"))
        );

        env.bind("map".into(), binding(Value::Int(3)));
        assert_eq!(
            crate::types::signature::lookup("map", &env, &sig),
            Some(&Value::Int(3))
        );

        env.unset("map");
        assert_eq!(
            crate::types::signature::lookup("map", &env, &sig),
            Some(&Value::string("prelude-map"))
        );
    }

    /// The same sequence of binds, shadowing included, gives equal
    /// lookups, `names()` and `restrict` results whether `SMALL` is crossed or
    /// not.
    #[test]
    fn small_and_large_agree() {
        let small = env_of([("a", 0), ("b", 1), ("c", 2)]);
        let mut large = small.clone();
        // Push `large` past `SMALL`, then shadow `a` again on both sides.
        for i in 0..(SMALL + 4) {
            let n = i64::try_from(i).expect("small test index");
            large.bind(format!("pad{i}").into(), binding(Value::Int(100 + n)));
        }
        let mut small = small;
        small.bind("a".into(), binding(Value::Int(9)));
        large.bind("a".into(), binding(Value::Int(9)));

        assert!(matches!(small.0, Entries::Small(_)));
        assert!(matches!(large.0, Entries::Large(_)));
        assert_eq!(small.get("a"), Some(&Value::Int(9)));
        assert_eq!(large.get("a"), Some(&Value::Int(9)));
        assert_eq!(small.get("b"), large.get("b"));
        assert_eq!(small.get("zzz"), None);
        assert_eq!(large.get("zzz"), None);

        let occ = crate::ir::test_occ(&["a", "b"]);
        let (mut small_names, mut large_names): (Vec<_>, Vec<_>) = (
            small
                .restrict(&occ)
                .names()
                .map(ToString::to_string)
                .collect(),
            large
                .restrict(&occ)
                .names()
                .map(ToString::to_string)
                .collect(),
        );
        small_names.sort();
        large_names.sort();
        assert_eq!(small_names, large_names);
    }

    /// An environment whose names occ covers returns itself
    /// (`ptr_eq`); one name more than occ lists gives a fresh `Small` of the
    /// hits; a name only Σ answers is never an entry.
    #[test]
    fn restrict_is_the_identity_when_it_drops_nothing() {
        let mut sig = Signature::default();
        sig.install_prelude(Arc::new(
            std::iter::once(("map".to_string(), binding(Value::string("p")))).collect(),
        ));
        let env = env_of([("a", 0), ("b", 1)]);

        let covering = crate::ir::test_occ(&["a", "b", "zzz"]);
        assert!(env.restrict(&covering).ptr_eq(&env));

        let narrower = crate::ir::test_occ(&["a"]);
        let narrowed = env.restrict(&narrower);
        assert!(!narrowed.ptr_eq(&env));
        assert!(narrowed.binding("a").is_some());
        assert!(narrowed.binding("b").is_none());

        let empty = crate::ir::test_occ(&[]);
        assert!(
            env.restrict(&empty).ptr_eq(&env.restrict(&empty)),
            "every empty capture shares one root"
        );
        assert!(
            !narrowed.names().any(|n| n.as_ref() == "map"),
            "a name only Σ answers is never an entry"
        );
    }
}
