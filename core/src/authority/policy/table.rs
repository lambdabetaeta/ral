//! Authority as a table of scoped evidence.
//!
//! A [`Table`] is two sets, one polarity each, and denotes a function from
//! subjects to [`Evidence`]: a subject has positive evidence where some
//! allowance contains it as stored, and negative evidence where some exclusion
//! matches it under any spelling of its name.  Nothing is ranked: an exclusion
//! beats every allowance, and only [`Evidence::permits`] closes a gap.  The fs
//! [`FsRules`](super::fs::FsRules) and the [`ExecRules`](super::exec::ExecRules)
//! are its two instances.

use super::{Evidence, Join, Meet};
use crate::path::MatchedBy;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt::Debug;

/// A positive scope: positional, names as stored.
///
/// The meet is exact under three obligations:
///
/// - every allowance contains its own subject;
/// - two allowances with a common subject are nested;
/// - `own()` detects containment: if `a` contains `k.own()` and `k` contains
///   `s`, then `a` contains `s`.
///
/// A future allowance that does not nest needs an explicit intersection before
/// it is added.
pub(crate) trait Allows: Ord + Clone + Debug {
    type Subject<'a>: Copy
    where
        Self: 'a;
    fn contains(&self, s: Self::Subject<'_>) -> bool;
    /// The subject this scope is itself judged at.  A scope that holds
    /// everywhere is judged at the universal subject, which only a scope that
    /// holds everywhere contains: so it survives a meet only with a table that
    /// also holds everywhere, as the root prefix does.
    fn own(&self) -> Self::Subject<'_>;
}

/// A negative scope: any spelling of a name.
pub(crate) trait Denies<A: Allows>: Ord + Clone + Debug {
    fn matches(&self, s: A::Subject<'_>) -> Option<MatchedBy>;
}

/// Why a supplied table refuses a subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Withheld<D> {
    /// No allowance contains it.
    MissingAllowance,
    /// An exclusion matches it, `matched_by` the least way any does.  Boxed:
    /// an exclusion holds its walks, which a refusal passes by value.
    Excluded {
        scope: Box<D>,
        matched_by: MatchedBy,
    },
}

/// Allowances and exclusions, each a set.  Bounds sit on the impls, not the
/// struct, so the derives bound nothing but the keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Table<A: Ord, D: Ord> {
    allows: BTreeSet<A>,
    denies: BTreeSet<D>,
}

impl<A: Ord, D: Ord> Default for Table<A, D> {
    fn default() -> Self {
        Self {
            allows: BTreeSet::new(),
            denies: BTreeSet::new(),
        }
    }
}

impl<A: Ord, D: Ord> Table<A, D> {
    pub(crate) fn new(
        allows: impl IntoIterator<Item = A>,
        denies: impl IntoIterator<Item = D>,
    ) -> Self {
        Self {
            allows: allows.into_iter().collect(),
            denies: denies.into_iter().collect(),
        }
    }

    pub(crate) fn allows(&self) -> impl Iterator<Item = &A> {
        self.allows.iter()
    }

    pub(crate) fn denies(&self) -> impl Iterator<Item = &D> {
        self.denies.iter()
    }

    /// Every exclusion taken hold of again by `rehold`.
    pub(crate) fn rehold_with(&mut self, rehold: impl Fn(&mut D)) {
        self.denies = std::mem::take(&mut self.denies)
            .into_iter()
            .map(|mut deny| {
                rehold(&mut deny);
                deny
            })
            .collect();
    }
}

impl<A: Allows, D: Denies<A>> Table<A, D> {
    pub(crate) fn supports(&self, s: A::Subject<'_>) -> bool {
        self.allows.iter().any(|a| a.contains(s))
    }

    pub(crate) fn excludes(&self, s: A::Subject<'_>) -> bool {
        self.denies.iter().any(|d| d.matches(s).is_some())
    }

    /// Computed, never stored: a table cannot hold a subject's evidence apart
    /// from its keys.
    pub(crate) fn evidence(&self, s: A::Subject<'_>) -> Evidence {
        Evidence {
            positive: self.supports(s),
            negative: self.excludes(s),
        }
    }

    /// Permitted at its own subject; descendants may still be excluded.
    pub(crate) fn live(&self) -> impl Iterator<Item = &A> {
        (self.allows.iter()).filter(move |a| self.evidence(a.own()).permits())
    }

    /// `None` exactly when `s` has positive evidence alone.  The exclusion
    /// cited is the one matching `s` the least way, in stable key order:
    /// deterministic, never by depth.
    pub(crate) fn withheld(&self, s: A::Subject<'_>) -> Option<Withheld<D>> {
        let witness = (self.denies.iter())
            .filter_map(|d| Some((d.matches(s)?, d)))
            .min_by_key(|&(matched_by, _)| matched_by);
        match witness {
            Some((matched_by, scope)) => Some(Withheld::Excluded {
                scope: Box::new(scope.clone()),
                matched_by,
            }),
            None => (!self.supports(s)).then_some(Withheld::MissingAllowance),
        }
    }
}

/// `∧`, pointwise on evidence: denies join; an allow survives where both
/// tables support its own subject.
impl<A: Allows, D: Denies<A>> Meet for Table<A, D> {
    fn meet(self, other: Self) -> Self {
        let allows = (self.allows.iter().chain(&other.allows))
            .filter(|a| self.supports(a.own()) && other.supports(a.own()))
            .cloned()
            .collect();
        let denies = self.denies.into_iter().chain(other.denies).collect();
        Self { allows, denies }
    }
}

/// `⊕`, pointwise on evidence: the baseline and support join through it.
impl<A: Allows, D: Denies<A>> Join for Table<A, D> {
    fn join(mut self, other: Self) -> Self {
        self.allows.extend(other.allows);
        self.denies.extend(other.denies);
        self
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::{Allows, Denies, Table};
    use crate::authority::policy::exec::tests::{Rng, paths, random};
    use crate::authority::policy::fs::{FsAllow, FsDeny, FsRules};
    use crate::authority::policy::{Evidence, Extent, GrantPath, Join, Meet};
    use crate::path::{Excluded, FrozenPath, RealPath, Subject, Trace};
    use crate::test_helper::{Tree, real, trace};
    use std::path::{Path, PathBuf};

    /// The three obligations of [`Allows`], at every subject given.
    fn obligations<'s, A: Allows + 's>(allows: &[A], subjects: &[A::Subject<'s>]) {
        for a in allows {
            assert!(a.contains(a.own()), "{a:?} does not hold its own subject");
            for k in allows {
                let shared = subjects.iter().any(|&s| a.contains(s) && k.contains(s));
                assert!(
                    !shared || a.contains(k.own()) || k.contains(a.own()),
                    "{a:?} and {k:?} share a subject and do not nest"
                );
                if a.contains(k.own()) {
                    for &s in subjects {
                        assert!(
                            !k.contains(s) || a.contains(s),
                            "{a:?} holds {k:?}'s own subject, and {k:?} a subject {a:?} does not"
                        );
                    }
                }
            }
        }
    }

    /// `ev(a ∧ b, s) = ev(a, s) ∧ ev(b, s)` and likewise for `⊕`, Both and
    /// Neither included, at every subject; and on what the meet makes, a
    /// semilattice, structurally: `a ∧ a = a`, commutative, associative; and
    /// likewise `⊕`, with `top` the meet's unit and the empty table the join's.
    /// The tables' allowances meet the three obligations that make the first
    /// exact.
    fn laws<'s, A: Allows + 's, D: Denies<A>>(
        mut rng: Rng,
        top: &Table<A, D>,
        random: impl Fn(&mut Rng) -> Table<A, D>,
        subjects: &[A::Subject<'s>],
    ) {
        for _ in 0..1000 {
            let (a, b, c) = (random(&mut rng), random(&mut rng), random(&mut rng));
            obligations(&a.allows().cloned().collect::<Vec<_>>(), subjects);
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
            assert_eq!(a.clone().join(a.clone()), a, "a = {a:?}");
            assert_eq!(
                joined.join(c.clone()),
                a.clone().join(b.clone().join(c.clone())),
                "a = {a:?}\nb = {b:?}\nc = {c:?}"
            );
            assert_eq!(top.clone().meet(a.clone()), a, "a = {a:?}");
            assert_eq!(a.clone().meet(top.clone()), a, "a = {a:?}");
            assert_eq!(a.clone().join(Table::default()), a, "a = {a:?}");
            assert_eq!(Table::default().join(a.clone()), a, "a = {a:?}");
        }
    }

    #[test]
    fn the_exec_meet_and_join_are_pointwise_on_evidence() {
        let paths = paths();
        let subjects: Vec<_> = crate::authority::policy::exec::tests::subjects(&paths).collect();
        let top = Table::new([crate::authority::policy::exec::ExecAllow::Any], []);
        laws(Rng(0x9e37_79b9_7f4a_7c15), &top, random, &subjects);
    }

    /// A tempdir tree with a symlink to a directory, a hard link and a renamed
    /// directory, and the real path of its root.
    fn tree() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = real(tmp.path()).as_path().to_path_buf();
        for dir in ["d/secrets", "e/sub", "old/inner"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("d/secrets/f"), "").unwrap();
        std::fs::hard_link(root.join("d/secrets/f"), root.join("d/hard")).unwrap();
        std::fs::rename(root.join("old"), root.join("moved")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("e"), root.join("link")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(root.join("e"), root.join("link")).unwrap();
        (tmp, root)
    }

    /// The tree's names as the allowance and the exclusion each makes: its
    /// root, a case pair of which one is absent, the link, the hard link, the
    /// renamed directory.
    fn prefixes(root: &Path) -> Vec<(GrantPath<Extent>, GrantPath<Excluded>)> {
        let names = [
            "d",
            "d/Secrets",
            "d/secrets",
            "d/secrets/f",
            "d/hard",
            "link",
            "e",
            "e/sub",
            "moved",
        ];
        std::iter::once(root.to_path_buf())
            .chain(names.map(|name| root.join(name)))
            .map(|path| FrozenPath::from_surface(crate::path::utf8(&path).unwrap()))
            .map(|path| {
                (
                    GrantPath::<Trace>::walk(path.clone()).unwrap(),
                    GrantPath::<Excluded>::walk(path).unwrap(),
                )
            })
            .collect()
    }

    /// Places beneath the tree's names, and one beside it.
    fn places(root: &Path) -> Vec<RealPath> {
        ["d", "d/secrets/f", "e/sub", "moved/inner", "z"]
            .map(|name| crate::test_helper::trace(root.join(name)).real().clone())
            .into()
    }

    /// A table over the tree's names, each present one time in three: as an
    /// allowance, an exclusion or both; the places; and everywhere.
    fn random_region(
        rng: &mut Rng,
        prefixes: &[(GrantPath<Extent>, GrantPath<Excluded>)],
        places: &[RealPath],
    ) -> FsRules {
        let (mut allows, mut denies) = (Vec::new(), Vec::new());
        for (allow, deny) in prefixes {
            if rng.below(3) == 0 {
                match rng.below(3) {
                    0 => allows.push(FsAllow::Prefix(allow.clone())),
                    1 => denies.push(FsDeny(deny.clone())),
                    _ => {
                        allows.push(FsAllow::Prefix(allow.clone()));
                        denies.push(FsDeny(deny.clone()));
                    }
                }
            }
        }
        for place in places {
            if rng.below(3) == 0 {
                allows.push(FsAllow::RunPlace(place.clone()));
            }
        }
        if rng.below(6) == 0 {
            allows.push(FsAllow::Prefix(GrantPath::everywhere()));
        }
        FsRules::new(allows, denies)
    }

    #[test]
    fn the_fs_meet_and_join_are_pointwise_on_evidence() {
        let (_tmp, root) = tree();
        let accesses: Vec<Trace> = [
            "d/secrets/f/x",
            "d/Secrets/x",
            "d/SECRETS/y",
            "d/hard",
            "d/x",
            "link/x",
            "e/x",
            "e/sub/x",
            "moved/inner",
            "z",
            "z/y",
        ]
        .map(|name| trace(root.join(name)))
        .into();
        let subjects: Vec<Subject<'_>> = (accesses.iter())
            .flat_map(|t| [Subject::Access(t), Subject::Position(t.real())])
            .chain([Subject::Everywhere])
            .collect();
        let top = Table::new([FsAllow::Prefix(GrantPath::everywhere())], []);
        let (prefixes, places) = (prefixes(&root), places(&root));
        let random = |rng: &mut Rng| random_region(rng, &prefixes, &places);
        laws(Rng(0x5851_f42d_4c95_7f2d), &top, random, &subjects);
    }

    /// The universal allowance is not privileged: it survives a meet only
    /// where the other table supports its own subject, so it narrows to what
    /// the other holds, and holds nothing against a table that holds nothing.
    #[test]
    fn the_universal_allowance_meets_as_any_other() {
        let nowhere = Tree::nowhere();
        let at = |p: &str| trace(nowhere.path(p));
        let everywhere = || FsRules::new([FsAllow::Prefix(GrantPath::everywhere())], []);
        let prefix = GrantPath::<Trace>::walk(FrozenPath::from_surface(&nowhere.at("/work")));
        let work = FsRules::new([FsAllow::Prefix(prefix.unwrap())], []);
        let met = everywhere().meet(work);
        assert!(met.evidence(Subject::Access(&at("/work"))).permits());
        assert!(met.evidence(Subject::Access(&at("/work/f"))).permits());
        assert!(!met.evidence(Subject::Access(&at("/secret"))).permits());
        let met = everywhere().meet(FsRules::default());
        let (work, secret) = (at("/work"), at("/secret"));
        for subject in [
            Subject::Access(&work),
            Subject::Access(&secret),
            Subject::Position(work.real()),
            Subject::Everywhere,
        ] {
            assert_eq!(met.evidence(subject), Evidence::NEITHER);
        }
    }
}
