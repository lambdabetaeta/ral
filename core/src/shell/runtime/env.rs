//! [`Env`], the finite map a value closure captures.

use crate::ir::{Name, Occ};
use crate::shell::runtime::ValClos;
use crate::shell::runtime::signature::Signature;
use crate::ty::Closed;
use either::Either;
use rustc_hash::FxBuildHasher;
use std::sync::Arc;
use triomphe::ThinArc;

/// A value and its scheme are installed together.
#[derive(Debug, Clone)]
pub struct Binding {
    pub value: ValClos,
    pub(crate) scheme: Option<Closed>,
}

type LargeMap =
    imbl::GenericHashMap<Name, Binding, FxBuildHasher, imbl::shared_ptr::DefaultSharedPtr>;
const SMALL: usize = 8;

#[derive(Debug, Clone)]
enum Entries {
    Empty,
    /// Sorted by name, distinct, one allocation, length in the header.
    Small(ThinArc<(), (Name, Binding)>),
    Sized(Arc<SizedEntries>),
}
#[derive(Debug, Clone)]
enum SizedEntries {
    SeedSmall(Vec<(Name, Binding)>),
    Large(LargeMap),
}
impl Entries {
    fn parts(&self) -> Either<&[(Name, Binding)], &LargeMap> {
        match self {
            Self::Empty => Either::Left(&[]),
            Self::Small(entries) => Either::Left(&entries.slice),
            Self::Sized(entries) => match entries.as_ref() {
                SizedEntries::SeedSmall(entries) => Either::Left(entries),
                SizedEntries::Large(entries) => Either::Right(entries),
            },
        }
    }
}
fn small_or_large(entries: Vec<(Name, Binding)>) -> Entries {
    if entries.is_empty() {
        Entries::Empty
    } else if entries.len() > SMALL {
        Entries::Sized(Arc::new(SizedEntries::Large(entries.into_iter().collect())))
    } else {
        Entries::Small(ThinArc::from_header_and_iter((), entries.into_iter()))
    }
}

/// ρ alone; [`super::signature::lookup`] adds Σ's fallback.
#[derive(Debug, Clone)]
pub struct Env(Entries);
const _: () = assert!(std::mem::size_of::<Env>() <= 16, "Env must fit in 16 bytes");

impl Env {
    pub const EMPTY: Self = Self(Entries::Empty);
    pub fn new() -> Self {
        Self::EMPTY
    }

    /// Retain displaced owners for seed cleanup.
    pub(crate) fn from_seed_bindings(bindings: Vec<(Name, Binding)>) -> (Self, Vec<Binding>) {
        let mut entries: Vec<(Name, Binding)> = Vec::with_capacity(bindings.len());
        let mut displaced = Vec::new();
        for (name, binding) in bindings {
            match entries.binary_search_by(|(n, _)| n.as_ref().cmp(name.as_ref())) {
                Ok(index) => {
                    let (_, previous) = std::mem::replace(&mut entries[index], (name, binding));
                    displaced.push(previous);
                }
                Err(index) => entries.insert(index, (name, binding)),
            }
        }
        let env = if entries.is_empty() {
            Self::EMPTY
        } else if entries.len() <= SMALL {
            Self(Entries::Sized(Arc::new(SizedEntries::SeedSmall(entries))))
        } else {
            Self(Entries::Sized(Arc::new(SizedEntries::Large(
                entries.into_iter().collect(),
            ))))
        };
        (env, displaced)
    }
    /// Only seed-created rows and their unmodified aliases are dismantled here.
    pub(crate) fn into_seed_bindings(self) -> Result<Vec<Binding>, Self> {
        match self.0 {
            Entries::Empty => Ok(Vec::new()),
            Entries::Sized(entries) => Ok(match Arc::into_inner(entries) {
                Some(SizedEntries::SeedSmall(entries)) => {
                    entries.into_iter().map(|(_, binding)| binding).collect()
                }
                Some(SizedEntries::Large(entries)) => {
                    entries.into_iter().map(|(_, binding)| binding).collect()
                }
                None => Vec::new(),
            }),
            Entries::Small(entries) => Err(Self(Entries::Small(entries))),
        }
    }
    pub(crate) fn drop_seed(self) {
        super::value::drop_seed_env(self);
    }
    pub(crate) fn len(&self) -> usize {
        match self.0.parts() {
            Either::Left(entries) => entries.len(),
            Either::Right(entries) => entries.len(),
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, name: &str) -> Option<&ValClos> {
        self.binding(name).map(|b| &b.value)
    }
    pub(crate) fn binding(&self, name: &str) -> Option<&Binding> {
        match self.0.parts() {
            Either::Left(entries) => entries
                .binary_search_by(|(n, _)| n.as_ref().cmp(name))
                .ok()
                .map(|i| &entries[i].1),
            Either::Right(entries) => entries.get(name),
        }
    }
    pub(crate) fn bind(&mut self, name: Name, binding: Binding) {
        if let Either::Left(a) = self.0.parts() {
            let (i, j) = match a.binary_search_by(|(n, _)| n.as_ref().cmp(name.as_ref())) {
                Ok(i) => (i, i + 1),
                Err(i) => (i, i),
            };
            if a.len() - (j - i) < SMALL {
                let len = a[..i].len() + 1 + a[j..].len();
                let mut entries = a[..i]
                    .iter()
                    .cloned()
                    .chain(std::iter::once((name, binding)))
                    .chain(a[j..].iter().cloned());
                // ThinArc requires ExactSizeIterator; Chain has only a size hint.
                self.0 = Entries::Small(ThinArc::from_header_and_iter(
                    (),
                    (0..len).map(|_| {
                        entries
                            .next()
                            .expect("the binding splice has its computed length")
                    }),
                ));
                return;
            }
        }
        self.extend(std::iter::once((name, binding)));
    }
    /// A later entry shadows an earlier one, in this call or already in ρ.
    pub(crate) fn extend(&mut self, entries: impl ExactSizeIterator<Item = (Name, Binding)>) {
        if entries.len() == 0 {
            return;
        }
        match self.0.parts() {
            Either::Left(a) => {
                let mut merged = Vec::with_capacity(a.len() + entries.len());
                merged.extend(a.iter().cloned());
                for (name, binding) in entries {
                    match merged.binary_search_by(|(n, _)| n.as_ref().cmp(name.as_ref())) {
                        Ok(i) => merged[i] = (name, binding),
                        Err(i) => merged.insert(i, (name, binding)),
                    }
                }
                self.0 = small_or_large(merged);
            }
            Either::Right(_) => {
                let Entries::Sized(sized) = &mut self.0 else {
                    unreachable!("large entries are sized")
                };
                let SizedEntries::Large(map) = Arc::make_mut(sized) else {
                    unreachable!("large entries")
                };
                for (name, binding) in entries {
                    map.insert(name, binding);
                }
            }
        }
    }
    pub(crate) fn unset(&mut self, name: &str) -> Option<ValClos> {
        match self.0.parts() {
            Either::Left(a) => {
                let i = a.binary_search_by(|(n, _)| n.as_ref().cmp(name)).ok()?;
                let mut kept = a.to_vec();
                let (_, binding) = kept.remove(i);
                self.0 = small_or_large(kept);
                Some(binding.value)
            }
            Either::Right(_) => {
                let Entries::Sized(sized) = &mut self.0 else {
                    unreachable!("large entries are sized")
                };
                let SizedEntries::Large(map) = Arc::make_mut(sized) else {
                    unreachable!("large entries")
                };
                Some(map.remove(name)?.value)
            }
        }
    }
    /// Restriction is the identity when it drops no binding.
    pub(crate) fn restrict(&self, occ: &Occ) -> Self {
        if occ.len() >= self.len() && self.names().all(|name| occ.contains(name)) {
            return self.clone();
        }
        let hits = occ
            .names()
            .filter_map(|n| self.binding(n).map(|b| (n.clone(), b.clone())))
            .collect();
        Self(small_or_large(hits))
    }
    pub(crate) fn names(&self) -> impl Iterator<Item = &Name> {
        self.iter().map(|(name, _)| name)
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Name, &Binding)> {
        match self.0.parts() {
            Either::Left(entries) => {
                Either::Left(entries.iter().map(|(name, binding)| (name, binding)))
            }
            Either::Right(entries) => Either::Right(entries.iter()),
        }
    }
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Entries::Empty, Entries::Empty) => true,
            (Entries::Small(a), Entries::Small(b)) => {
                std::ptr::eq(&raw const a.slice, &raw const b.slice)
            }
            (Entries::Sized(a), Entries::Sized(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
    /// ρ then Σ's prelude, projecting each name on first sight.
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
    pub(crate) fn largest_shallow_size(&self, sig: &Signature) -> usize {
        let session = self.iter().map(|(_, b)| b.value.shallow_size());
        let prelude = sig
            .prelude_names()
            .filter(|name| self.binding(name).is_none())
            .filter_map(|name| sig.prelude_binding(name))
            .map(|b| b.value.shallow_size());
        session.chain(prelude).max().unwrap_or(0)
    }
    pub(crate) fn all_bindings(&self, sig: &Signature) -> Vec<(String, ValClos)> {
        self.fold_union(sig, |b| b.value.clone())
    }
    pub(crate) fn distinct_name_count(&self, sig: &Signature) -> usize {
        let mut seen: std::collections::HashSet<&str> = self.names().map(AsRef::as_ref).collect();
        seen.extend(sig.prelude_names());
        seen.len()
    }
    pub(crate) fn binding_schemes(&self, sig: &Signature) -> Vec<(String, Option<Closed>)> {
        self.fold_union(sig, |b| b.scheme.clone())
    }
}
impl Default for Env {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding(value: ValClos) -> Binding {
        Binding {
            value,
            scheme: None,
        }
    }
    fn env_of<const N: usize>(pairs: [(&str, i64); N]) -> Env {
        let mut env = Env::EMPTY;
        for (name, n) in pairs {
            env.bind(name.into(), binding(ValClos::int(n)));
        }
        env
    }
    #[test]
    fn seed_rows_preserve_iteration_wire_order_and_identity() {
        for count in [0, 1, SMALL, SMALL + 1] {
            for duplicate in [false, true] {
                let mut pairs: Vec<(Name, Binding)> = (0..count)
                    .rev()
                    .map(|index| (format!("key{index:02}").into(), binding(ValClos::int(1))))
                    .collect();
                if duplicate && count > 0 {
                    pairs.push(("key00".into(), binding(ValClos::int(2))));
                }
                let mut ordinary = Env::EMPTY;
                ordinary.extend(pairs.clone().into_iter());
                let (seed, displaced) = Env::from_seed_bindings(pairs);
                assert_eq!(displaced.len(), usize::from(duplicate && count > 0));
                let wire = |env: &Env| {
                    let mut shell = crate::test_helper::core_shell();
                    shell.env = env.clone();
                    let seed = crate::seed::EngineSeed::encode(
                        &shell.bequeath(crate::shell::grant::SpawnGrant::Inherit),
                    );
                    let bytes = crate::data::json::to_vec(&seed).unwrap();
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["scope_table"]
                        .clone()
                };
                assert_eq!(wire(&ordinary), wire(&seed));
                assert_eq!(
                    ordinary.names().collect::<Vec<_>>(),
                    seed.names().collect::<Vec<_>>()
                );
                let clone = seed.clone();
                assert!(seed.ptr_eq(&clone));
                let names: Vec<&str> = seed.names().map(AsRef::as_ref).collect();
                let restricted = seed.restrict(&crate::ir::test_occ(&names));
                assert!(seed.ptr_eq(&restricted));
                restricted.drop_seed();
                seed.drop_seed();
                clone.drop_seed();
            }
        }
    }
    #[test]
    fn deep_closure_chain_drops_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                drop(crate::shell::runtime::deep_block_chain(
                    100_000,
                    ValClos::UNIT,
                ));
            })
            .expect("spawn")
            .join()
            .expect("a deep chain drops without exhausting the stack");
    }
    #[test]
    fn shadowed_prelude_name_round_trips_then_unset_reveals_it() {
        let mut sig = Signature::default();
        sig.install_prelude(Arc::new(
            std::iter::once(("map".to_string(), binding(ValClos::string("prelude-map")))).collect(),
        ));
        let mut env = Env::EMPTY;
        assert_eq!(
            crate::shell::runtime::signature::lookup("map", &env, &sig),
            Some(&ValClos::string("prelude-map"))
        );
        env.bind("map".into(), binding(ValClos::int(3)));
        assert_eq!(
            crate::shell::runtime::signature::lookup("map", &env, &sig),
            Some(&ValClos::int(3))
        );
        env.unset("map");
        assert_eq!(
            crate::shell::runtime::signature::lookup("map", &env, &sig),
            Some(&ValClos::string("prelude-map"))
        );
    }
    #[test]
    fn small_and_large_agree() {
        let mut small = env_of([("a", 0), ("b", 1), ("c", 2)]);
        let mut large = small.clone();
        for i in 0..(SMALL + 4) {
            let n = i64::try_from(i).expect("small test index");
            large.bind(format!("pad{i}").into(), binding(ValClos::int(100 + n)));
        }
        small.bind("a".into(), binding(ValClos::int(9)));
        large.bind("a".into(), binding(ValClos::int(9)));
        assert!(matches!(small.0, Entries::Small(_)));
        assert!(
            matches!(&large.0, Entries::Sized(entries) if matches!(entries.as_ref(), SizedEntries::Large(_)))
        );
        assert_eq!(small.get("a"), Some(&ValClos::int(9)));
        assert_eq!(large.get("a"), Some(&ValClos::int(9)));
        assert_eq!(small.get("b"), large.get("b"));
        assert_eq!(small.get("zzz"), None);
        assert_eq!(large.get("zzz"), None);
        let occ = crate::ir::test_occ(&["a", "b"]);
        let names = |env: &Env| {
            let mut names: Vec<_> = env
                .restrict(&occ)
                .names()
                .map(ToString::to_string)
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&small), names(&large));
    }
    #[test]
    fn restrict_is_the_identity_when_it_drops_nothing() {
        let env = env_of([("a", 0), ("b", 1)]);
        assert!(
            env.restrict(&crate::ir::test_occ(&["a", "b", "zzz"]))
                .ptr_eq(&env)
        );
        let narrowed = env.restrict(&crate::ir::test_occ(&["a"]));
        assert!(!narrowed.ptr_eq(&env));
        assert!(narrowed.binding("a").is_some());
        assert!(narrowed.binding("b").is_none());
        let empty = env.restrict(&crate::ir::test_occ(&[]));
        assert!(empty.ptr_eq(&Env::EMPTY));
        assert!(matches!(empty.0, Entries::Empty));
        assert!(!narrowed.names().any(|n| n.as_ref() == "map"));
    }
}
