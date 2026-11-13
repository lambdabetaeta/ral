//! [`Env`], the finite map a value closure captures.

use crate::ir::{Name, Occ, Val, ValClos, synthetic};
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
    Large(Arc<LargeMap>),
}
impl Entries {
    fn parts(&self) -> Either<&[(Name, Binding)], &LargeMap> {
        match self {
            Self::Empty => Either::Left(&[]),
            Self::Small(entries) => Either::Left(&entries.slice),
            Self::Large(entries) => Either::Right(entries),
        }
    }
}
fn small_or_large(entries: Vec<(Name, Binding)>) -> Entries {
    if entries.is_empty() {
        Entries::Empty
    } else if entries.len() > SMALL {
        Entries::Large(Arc::new(entries.into_iter().collect()))
    } else {
        Entries::Small(ThinArc::from_header_and_iter((), entries.into_iter()))
    }
}

/// ρ alone; Σ's fallback is `shell::runtime::lookup`.
#[derive(Debug, Clone)]
pub struct Env(Entries);
const _: () = assert!(std::mem::size_of::<Env>() <= 16, "Env must fit in 16 bytes");

impl Env {
    pub const EMPTY: Self = Self(Entries::Empty);
    pub fn new() -> Self {
        Self::EMPTY
    }

    /// The allocation's identity while it lives; `None` for ∅.
    pub(crate) fn addr(&self) -> Option<std::num::NonZeroUsize> {
        let addr = match &self.0 {
            Entries::Empty => return None,
            Entries::Small(entries) => entries.slice.as_ptr().addr(),
            Entries::Large(entries) => Arc::as_ptr(entries).addr(),
        };
        std::num::NonZeroUsize::new(addr)
    }
    /// Refused with the first name bound twice.
    pub(crate) fn from_bindings(bindings: Vec<(Name, Binding)>) -> Result<Self, Name> {
        let mut seen = std::collections::HashSet::with_capacity(bindings.len());
        for (name, _) in &bindings {
            if !seen.insert(name) {
                return Err(name.clone());
            }
        }
        let mut bindings = bindings;
        bindings.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        Ok(Self(small_or_large(bindings)))
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
                let Entries::Large(map) = &mut self.0 else {
                    unreachable!("large entries")
                };
                let map = Arc::make_mut(map);
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
                let Entries::Large(map) = &mut self.0 else {
                    unreachable!("large entries")
                };
                let map = Arc::make_mut(map);
                let value = map.remove(name)?.value;
                if map.is_empty() {
                    self.0 = Entries::Empty;
                }
                Some(value)
            }
        }
    }
    /// `value` as an element of syntax closed over `self`: an atom is inlined;
    /// anything else is bound here under a fresh name.
    pub(crate) fn place(&mut self, value: ValClos) -> Val {
        value.into_inline().unwrap_or_else(|value| {
            let name = self.fresh_item();
            self.bind(
                name.clone(),
                Binding {
                    value,
                    scheme: None,
                },
            );
            Val::Variable(name)
        })
    }
    /// The first `%itemK`, K ≥ |ρ|, that ρ does not bind. At most |ρ| of the
    /// candidates are bound, so the search ends within |ρ| + 1 steps.
    fn fresh_item(&self) -> Name {
        (self.len()..=2 * self.len())
            .map(|k| synthetic("item", k))
            .find(|name| self.binding(name).is_none())
            .expect("|ρ| + 1 candidates, at most |ρ| of them bound")
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
    /// Whether both are one allocation (or both ∅).
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Entries::Empty, Entries::Empty) => true,
            (Entries::Small(a), Entries::Small(b)) => {
                std::ptr::eq(&raw const a.slice, &raw const b.slice)
            }
            (Entries::Large(a), Entries::Large(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
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
    fn removing_every_binding_leaves_no_allocation_identity() {
        let mut env = Env::from_bindings(
            (0..=SMALL)
                .map(|index| (format!("key{index}").into(), binding(ValClos::UNIT)))
                .collect(),
        )
        .unwrap();
        for index in 0..=SMALL {
            assert!(env.unset(&format!("key{index}")).is_some());
        }
        assert!(env.is_empty());
        assert_eq!(env.addr(), None);
    }
    #[test]
    fn from_bindings_agrees_with_extend_and_refuses_the_first_duplicate() {
        for count in [0, 1, SMALL, SMALL + 1] {
            let pairs: Vec<(Name, Binding)> = (0..count)
                .rev()
                .map(|index| {
                    (
                        format!("key{index:02}").into(),
                        binding(ValClos::int(i64::try_from(index).unwrap())),
                    )
                })
                .collect();
            let mut ordinary = Env::EMPTY;
            ordinary.extend(pairs.clone().into_iter());
            let env = Env::from_bindings(pairs.clone()).unwrap();
            assert_eq!(env.len(), ordinary.len());
            for (name, binding) in ordinary.iter() {
                assert_eq!(env.get(name), Some(&binding.value));
            }
            assert_eq!(env.addr().is_none(), count == 0);
            assert_eq!(env.addr(), env.clone().addr());
            assert_eq!(
                env.addr(),
                env.restrict(&crate::ir::test_occ(
                    &env.names().map(AsRef::as_ref).collect::<Vec<_>>(),
                ))
                .addr()
            );
            if let Some((name, binding)) = pairs.first() {
                let mut duplicate = pairs.clone();
                duplicate.extend([
                    (name.clone(), binding.clone()),
                    ("later".into(), binding.clone()),
                    ("later".into(), binding.clone()),
                ]);
                assert_eq!(Env::from_bindings(duplicate).unwrap_err(), *name);
            }
        }
    }
    #[test]
    fn deep_closure_chain_drops_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                drop(crate::ir::deep_block_chain(100_000, ValClos::UNIT));
            })
            .expect("spawn")
            .join()
            .expect("a deep chain drops without exhausting the stack");
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
        assert!(matches!(large.0, Entries::Large(_)));
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
    #[test]
    fn placing_inlines_atoms_and_binds_everything_else_under_fresh_names() {
        let mut env = Env::EMPTY;
        for atom in [
            ValClos::UNIT,
            ValClos::bool(true),
            ValClos::int(1),
            ValClos::string("s"),
            ValClos::bytes(b"b".to_vec()),
        ] {
            assert!(!matches!(env.place(atom), Val::Variable(_)));
        }
        assert!(env.is_empty());
        let closed_list = ValClos::list(vec![]);
        let closed_map = ValClos::map(vec![]);
        let tagged = ValClos::variant("t", None);
        let block = crate::ir::block_over(&Env::EMPTY);
        let handle = ValClos::withheld("h");
        for (i, value) in [closed_list, closed_map, tagged, block, handle]
            .into_iter()
            .enumerate()
        {
            let Val::Variable(name) = env.place(value) else {
                panic!("only atoms are inlined")
            };
            assert_eq!(name.as_ref(), format!("%item{i}"));
            assert!(env.binding(&name).is_some());
        }
    }
    #[test]
    fn a_fresh_name_skips_the_ones_in_use() {
        let mut env = env_of([("%item1", 0), ("%item2", 0)]);
        let Val::Variable(name) = env.place(ValClos::withheld("h")) else {
            panic!("a handle is bound")
        };
        assert_eq!(name.as_ref(), "%item3");
        assert_eq!(env.len(), 3);
    }
    #[test]
    fn binding_into_a_large_env_leaves_the_original_untouched() {
        let large = Env::from_bindings(
            (0..=SMALL * 4)
                .map(|index| (format!("key{index:02}").into(), binding(ValClos::UNIT)))
                .collect(),
        )
        .unwrap();
        let mut grown = large.clone();
        grown.bind("extra".into(), binding(ValClos::int(1)));
        assert_eq!(grown.len(), large.len() + 1);
        assert!(large.binding("extra").is_none());
        assert!(grown.binding("key00").is_some());
    }
}
