//! Authority as a table of scoped rules.
//!
//! A [`Table`] maps scopes to verdicts and denotes a function from subjects
//! to verdicts: the most specific rule that speaks to a subject decides,
//! ties meet, and silence denies.  The fs [`Region`](super::fs::Region) and
//! the [`ExecRules`](super::exec::ExecRules) are its two instances.

use crate::path::{Allow, Deny, Polarity};
use crate::types::{Meet, Verdict, meet_insert};
use std::collections::BTreeMap;

/// Where a rule applies.
pub(crate) trait Scope: Ord + Clone {
    type Subject<'a>: Copy
    where
        Self: 'a;
    type Rank: Ord;
    /// How specific a rule here is; the greater decides.
    fn rank(&self, verdict: &Verdict) -> Self::Rank;
    /// Whether `subject` falls in this scope, names read as `P` reads them.
    fn holds<P: Polarity>(&self, subject: Self::Subject<'_>) -> bool;
    /// The subject a rule at this scope is itself judged at.
    fn own(&self) -> Self::Subject<'_>;
}

/// Rules by scope; a scope written twice meets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Table<K: Scope>(BTreeMap<K, Verdict>);

impl<K: Scope> Default for Table<K> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<K: Scope> Extend<(K, Verdict)> for Table<K> {
    fn extend<I: IntoIterator<Item = (K, Verdict)>>(&mut self, rules: I) {
        for (scope, verdict) in rules {
            meet_insert(&mut self.0, scope, verdict);
        }
    }
}

impl<K: Scope> FromIterator<(K, Verdict)> for Table<K> {
    fn from_iter<I: IntoIterator<Item = (K, Verdict)>>(rules: I) -> Self {
        let mut table = Self::default();
        table.extend(rules);
        table
    }
}

impl<K: Scope> Table<K> {
    pub(crate) fn rules(&self) -> impl Iterator<Item = (&K, &Verdict)> {
        self.0.iter()
    }

    pub(crate) fn verdict(&self, subject: K::Subject<'_>) -> Verdict {
        most_specific(self.speaking(subject))
    }

    /// The most specific deny holding `subject` only under another spelling
    /// of its name, for a refusal to cite; none when some deny holds it as
    /// stored.
    pub(crate) fn respelled(&self, subject: K::Subject<'_>) -> Option<&K> {
        if self.denies().any(|k| k.holds::<Allow>(subject)) {
            return None;
        }
        self.denies()
            .filter(|k| k.holds::<Deny>(subject))
            .max_by_key(|k| k.rank(&Verdict::Deny))
    }

    /// The allows still in force at their own subjects.
    pub(crate) fn live(&self) -> impl Iterator<Item = &K> {
        (self.0.iter())
            .filter(|(k, v)| !v.is_denied() && !self.verdict(k.own()).is_denied())
            .map(|(k, _)| k)
    }

    pub(crate) fn denies(&self) -> impl Iterator<Item = &K> {
        (self.0.iter())
            .filter(|(_, v)| v.is_denied())
            .map(|(k, _)| k)
    }

    fn speaking<'s>(
        &'s self,
        subject: K::Subject<'_>,
    ) -> impl Iterator<Item = (K::Rank, &'s Verdict)> {
        (self.0.iter())
            .filter(move |(k, v)| speaks(*k, v, subject))
            .map(|(k, v)| (k.rank(v), v))
    }
}

/// Whether a rule speaks to `subject`.  A table names polarities, never
/// identities: which spellings are one name is `path`'s alone to say.
pub(super) fn speaks<K: Scope>(scope: &K, verdict: &Verdict, subject: K::Subject<'_>) -> bool {
    match verdict {
        Verdict::Deny => scope.holds::<Deny>(subject),
        _ => scope.holds::<Allow>(subject),
    }
}

/// The greatest rank decides; equal ranks meet; silence denies.
fn most_specific<'v, R: Ord>(speakers: impl IntoIterator<Item = (R, &'v Verdict)>) -> Verdict {
    speakers
        .into_iter()
        .fold(None::<(R, Verdict)>, |best, (rank, v)| match best {
            Some((held, b)) if held > rank => Some((held, b)),
            Some((held, b)) if held == rank => Some((held, b.meet(v.clone()))),
            _ => Some((rank, v.clone())),
        })
        .map_or(Verdict::Deny, |(_, v)| v)
}

/// `⟦a ∧ b⟧ = ⟦a⟧ ∧ ⟦b⟧`: denies join; an allow survives at its own
/// subject's met verdict, where that is not a deny.
impl<K: Scope> Meet for Table<K> {
    fn meet(self, other: Self) -> Self {
        let both = |k: &K| self.verdict(k.own()).meet(other.verdict(k.own()));
        // A default met to a deny stays absent: written down, a deny holds
        // every spelling of its name.
        (self.0.iter().chain(&other.0))
            .filter_map(|(k, v)| {
                let m = if v.is_denied() {
                    Verdict::Deny
                } else {
                    both(k)
                };
                (v.is_denied() || !m.is_denied()).then(|| (k.clone(), m))
            })
            .collect()
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
    use crate::capability::fs::Region;
    use crate::path::FrozenPath;
    use crate::types::{Meet, Verdict};
    use std::path::Path;

    /// `⟦a ∧ b⟧ = ⟦a⟧ ∧ ⟦b⟧` at every subject, and the meet is a semilattice
    /// on what it makes: a table as written may hold an allow its own rules
    /// outrank, which a meet leaves out.
    fn laws<'s, K: Scope + std::fmt::Debug + 's>(
        mut rng: Rng,
        random: impl Fn(&mut Rng) -> Table<K>,
        subjects: &[K::Subject<'s>],
    ) {
        for _ in 0..1000 {
            let (a, b, c) = (random(&mut rng), random(&mut rng), random(&mut rng));
            let met = a.clone().meet(b.clone());
            for &s in subjects {
                let pointwise = a.verdict(s).meet(b.verdict(s));
                assert_eq!(met.verdict(s), pointwise, "a = {a:?}\nb = {b:?}");
            }
            let own = a.clone().meet(a.clone());
            assert_eq!(own.clone().meet(own.clone()), own, "a = {a:?}");
            assert_eq!(met, b.clone().meet(a.clone()), "a = {a:?}\nb = {b:?}");
            assert_eq!(
                met.meet(c.clone()),
                a.clone().meet(b.clone().meet(c.clone())),
                "a = {a:?}\nb = {b:?}\nc = {c:?}"
            );
        }
    }

    #[test]
    fn the_exec_meet_is_pointwise_and_a_semilattice() {
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

    fn random_region(rng: &mut Rng) -> Region {
        (prefixes().into_iter())
            .filter_map(|p| (rng.below(3) == 0).then(|| (p, Verdict::from(rng.below(2) == 0))))
            .collect()
    }

    #[test]
    fn the_fs_meet_is_pointwise_and_a_semilattice() {
        let probes = [
            "/d/secrets/f/x",
            "/d/Secrets/x",
            "/d/SECRETS/y",
            "/d/x",
            "/d/link/x",
            "/e/x",
            "/e/sub/x",
            "/z",
        ]
        .map(Path::new);
        laws(Rng(0x5851_f42d_4c95_7f2d), random_region, &probes);
    }
}
