//! Authority as a table of scoped evidence.
//!
//! A [`Table`] maps scopes to [`Evidence`] and denotes a function from
//! subjects to it: a subject has positive evidence where some positive scope
//! holds it as stored, and negative evidence where some negative scope holds
//! it under any spelling of its name.  Nothing is ranked: an exclusion beats
//! every allowance, and only [`Evidence::permits`] closes a gap.  The fs
//! [`Region`](super::fs::Region) and the [`ExecRules`](super::exec::ExecRules)
//! are its two instances.

use super::lattice::join_insert;
use super::{Evidence, Join, Meet};
use crate::path::{Allow, Deny, Polarity};
use std::collections::BTreeMap;

/// Where a rule applies.
///
/// The meet is exact under three obligations:
///
/// - every positive scope holds its own subject;
/// - two positive scopes with a common subject are nested;
/// - `own()` detects containment: if `a` holds `own(k)` and `k` holds `s`,
///   then `a` holds `s`.
///
/// A future positive scope that does not nest needs an explicit intersection
/// before it is added.
pub(crate) trait Scope: Ord + Clone + std::fmt::Debug {
    type Subject<'a>: Copy
    where
        Self: 'a;
    /// Whether `subject` falls in this scope, names read as `P` reads them.
    fn holds<P: Polarity>(&self, subject: Self::Subject<'_>) -> bool;
    /// The subject a rule at this scope is itself judged at.
    fn own(&self) -> Self::Subject<'_>;
    /// Whether `evidence` may be stored here: a scope has one polarity by
    /// construction where its subjects are not nested.
    fn stores(&self, _evidence: Evidence) -> bool {
        true
    }
}

/// Why a supplied table refuses a subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Withheld<K> {
    /// No scope supports it.
    MissingAllowance,
    /// An exclusion applies; `respelled` when it holds the subject only under
    /// another spelling of its name.
    Excluded { scope: K, respelled: bool },
}

/// Evidence by scope; a scope written twice joins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Table<K: Scope>(BTreeMap<K, Evidence>);

impl<K: Scope> Default for Table<K> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<K: Scope> Extend<(K, Evidence)> for Table<K> {
    fn extend<I: IntoIterator<Item = (K, Evidence)>>(&mut self, rules: I) {
        for (scope, evidence) in rules {
            debug_assert!(scope.stores(evidence), "{scope:?}");
            join_insert(&mut self.0, scope, evidence);
        }
    }
}

impl<K: Scope> FromIterator<(K, Evidence)> for Table<K> {
    fn from_iter<I: IntoIterator<Item = (K, Evidence)>>(rules: I) -> Self {
        let mut table = Self::default();
        table.extend(rules);
        table
    }
}

impl<K: Scope> Table<K> {
    pub(crate) fn rules(&self) -> impl Iterator<Item = (&K, &Evidence)> {
        self.0.iter()
    }

    pub(crate) fn supports(&self, s: K::Subject<'_>) -> bool {
        self.0
            .iter()
            .any(|(k, e)| e.positive && k.holds::<Allow>(s))
    }

    pub(crate) fn excludes(&self, s: K::Subject<'_>) -> bool {
        self.0.iter().any(|(k, e)| e.negative && k.holds::<Deny>(s))
    }

    pub(crate) fn evidence(&self, s: K::Subject<'_>) -> Evidence {
        Evidence {
            positive: self.supports(s),
            negative: self.excludes(s),
        }
    }

    pub(crate) fn positives(&self) -> impl Iterator<Item = &K> {
        (self.0.iter()).filter(|(_, e)| e.positive).map(|(k, _)| k)
    }

    pub(crate) fn denies(&self) -> impl Iterator<Item = &K> {
        (self.0.iter()).filter(|(_, e)| e.negative).map(|(k, _)| k)
    }

    /// Permitted at its own subject; descendants may still be excluded.
    pub(crate) fn live(&self) -> impl Iterator<Item = &K> {
        self.positives()
            .filter(move |k| self.evidence(k.own()).permits())
    }

    /// `None` exactly when `s` has positive evidence alone.  An exclusion
    /// that holds `s` as stored is cited before one that holds it only by a
    /// fold, in stable key order: deterministic, never by depth.
    pub(crate) fn withheld(&self, s: K::Subject<'_>) -> Option<Withheld<K>> {
        let witness = self
            .denies()
            .filter(|k| k.holds::<Deny>(s))
            .min_by_key(|k| !k.holds::<Allow>(s));
        match witness {
            Some(k) => Some(Withheld::Excluded {
                scope: k.clone(),
                respelled: !k.holds::<Allow>(s),
            }),
            None => (!self.supports(s)).then_some(Withheld::MissingAllowance),
        }
    }
}

/// `∧`, pointwise on evidence: denies join; an allow survives where both
/// tables support its own subject.
impl<K: Scope> Meet for Table<K> {
    fn meet(self, other: Self) -> Self {
        let denies = (self.denies().chain(other.denies())).map(|k| (k.clone(), Evidence::DENY));
        let allows = (self.positives().chain(other.positives()))
            .filter(|k| self.supports(k.own()) && other.supports(k.own()))
            .map(|k| (k.clone(), Evidence::ALLOW));
        denies.chain(allows).collect()
    }
}

/// `⊕`, pointwise on evidence: the floor and support join through it.
impl<K: Scope> Join for Table<K> {
    fn join(mut self, other: Self) -> Self {
        self.extend(other.0);
        self
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test probes are lexical paths already in normal form"
)]
mod tests {
    use super::{Scope, Table};
    use crate::capability::exec::tests::{Rng, paths, random};
    use crate::capability::fs::{FsScope, Region};
    use crate::capability::{Evidence, Join, Meet};
    use crate::path::{FrozenPath, RealPath};
    use std::path::Path;

    /// `ev(a ∧ b, s) = ev(a, s) ∧ ev(b, s)` and likewise for `⊕`, Both and
    /// Neither included, at every subject; and on what the meet makes, a
    /// semilattice, structurally: `a ∧ a = a`, commutative, associative.
    fn laws<'s, K: Scope + std::fmt::Debug + 's>(
        mut rng: Rng,
        random: impl Fn(&mut Rng) -> Table<K>,
        subjects: &[K::Subject<'s>],
    ) {
        for _ in 0..1000 {
            let (a, b, c) = (random(&mut rng), random(&mut rng), random(&mut rng));
            let met = a.clone().meet(b.clone());
            let joined = a.clone().join(b.clone());
            for &s in subjects {
                assert_eq!(
                    met.evidence(s),
                    a.evidence(s).meet(b.evidence(s)),
                    "∧\na = {a:?}\nb = {b:?}"
                );
                assert_eq!(
                    joined.evidence(s),
                    a.evidence(s).join(b.evidence(s)),
                    "⊕\na = {a:?}\nb = {b:?}"
                );
            }
            assert_eq!(a.clone().meet(a.clone()), a, "a = {a:?}");
            assert_eq!(met, b.clone().meet(a.clone()), "a = {a:?}\nb = {b:?}");
            assert_eq!(
                met.meet(c.clone()),
                a.clone().meet(b.clone().meet(c.clone())),
                "a = {a:?}\nb = {b:?}\nc = {c:?}"
            );
            assert_eq!(joined, b.clone().join(a.clone()), "a = {a:?}\nb = {b:?}");
        }
    }

    #[test]
    fn the_exec_meet_and_join_are_pointwise_on_evidence() {
        let paths = paths();
        let subjects: Vec<_> = crate::capability::exec::tests::subjects(&paths).collect();
        laws(Rng(0x9e37_79b9_7f4a_7c15), random, &subjects);
    }

    /// A case pair, a symlink-divergent pair, the root and nested dirs.
    fn prefixes() -> [FrozenPath; 8] {
        let lit = |s: &str| FrozenPath::for_test(s, s);
        [
            lit("/"),
            lit("/d"),
            lit("/d/Secrets"),
            lit("/d/secrets"),
            lit("/d/secrets/f"),
            FrozenPath::for_test("/d/link", "/e"),
            lit("/e"),
            lit("/e/sub"),
        ]
    }

    /// Places at a prefix's path and at its descendants.
    fn places() -> [RealPath; 4] {
        ["/d", "/d/secrets/f", "/e/sub", "/z"].map(RealPath::assumed)
    }

    fn random_region(rng: &mut Rng) -> Region {
        let mut region = Region::default();
        for prefix in prefixes() {
            if rng.below(3) == 0 {
                let evidence = rng.pick(&[Evidence::ALLOW, Evidence::DENY, Evidence::BOTH]);
                region.extend([(FsScope::Prefix(prefix), evidence)]);
            }
        }
        for place in places() {
            if rng.below(3) == 0 {
                region.extend([(FsScope::Place(place), Evidence::ALLOW)]);
            }
        }
        region
    }

    #[test]
    fn the_fs_meet_and_join_are_pointwise_on_evidence() {
        let probes = [
            "/d/secrets/f/x",
            "/d/Secrets/x",
            "/d/SECRETS/y",
            "/d/x",
            "/d/link/x",
            "/e/x",
            "/e/sub/x",
            "/z",
            "/z/y",
        ]
        .map(Path::new);
        laws(Rng(0x5851_f42d_4c95_7f2d), random_region, &probes);
    }
}
