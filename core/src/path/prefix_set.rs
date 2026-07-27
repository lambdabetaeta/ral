//! A set of grant prefixes, deduplicated and unioned/intersected as a
//! whole.
//!
//! Each element is a [`NormalizedPrefix`], already carrying its
//! `surface` and `resolved` forms — this module contributes the
//! *set*-level operations over them, not the per-prefix normal form.
//! [`meet_prefixes`] is the containment kernel: a prefix survives an
//! intersection iff some prefix on the other side covers it (deeper
//! wins), judged on `(namespace, resolved)` so a symlinked deeper
//! prefix cannot escape a shallower ceiling and a cross-namespace meet
//! is the empty, fail-closed intersection.  It is pure — no disk, no
//! `Resolver` — which is what lets [`PrefixSet::meet`] and every
//! lattice meet in `types::capability` share it.
//!
//! [`PrefixSet::resolve`] is the one door here that still touches disk:
//! the sandbox-projection fold holds a live [`Resolver`] and needs the
//! resolved form of a prefix that may not be frozen yet (a bare
//! exec-dir string, a `~`-headed fs prefix).  That is enforcement-
//! adjacent rendering work, not composition, so it keeps consulting
//! the world — see `design/260727_policy_kernel_purity.md` §0.

use super::lex::path_within_str;
use super::resolved::NormalizedPrefix;
use super::resolver::Resolver;
use crate::types::Meet;

/// The one containment judgment: does `a` cover `b`?
///
/// Same namespace, `b`'s resolved form within `a`'s: two prefixes in
/// different namespaces never overlap, and a symlinked `b` cannot
/// escape a shallower `a` because this reads the resolved form, never
/// the surface spelling.  Pure — no disk, no `Resolver`.
pub fn covers(a: &NormalizedPrefix, b: &NormalizedPrefix) -> bool {
    a.namespace() == b.namespace() && path_within_str(b.resolved(), a.resolved())
}

/// Intersect two prefix lists, keeping every element covered by some
/// element of the other list — i.e. the deeper prefix of each
/// overlapping pair survives.
///
/// Pure — no disk, no `Resolver` — so [`PrefixSet::meet`] and every
/// lattice meet in `types::capability` can share this one kernel.  The
/// result is unsorted and may contain duplicates; callers dedup.
pub fn meet_prefixes(a: &[NormalizedPrefix], b: &[NormalizedPrefix]) -> Vec<NormalizedPrefix> {
    let is_covered =
        |x: &NormalizedPrefix, others: &[NormalizedPrefix]| others.iter().any(|o| covers(o, x));
    a.iter()
        .filter(|x| is_covered(x, b))
        .cloned()
        .chain(b.iter().filter(|y| is_covered(y, a)).cloned())
        .collect()
}

/// A sorted, deduplicated set of [`NormalizedPrefix`]es.  `Default` is
/// the empty set, the identity for [`union`](PrefixSet::union).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrefixSet(Vec<NormalizedPrefix>);

impl PrefixSet {
    /// Resolve each prefix through `resolver` — sigil/`~` expansion,
    /// `.`/`..` normalisation, then symlink-following — and mint the
    /// pair.  Accepts either the grant-side [`NormalizedPrefix`]es or
    /// the bare exec-dir strings; a `NormalizedPrefix` is idempotent
    /// under the fold, so re-resolving one already frozen is a no-op
    /// past the first step.
    pub fn resolve<S: AsRef<str>>(resolver: &Resolver, prefixes: &[S]) -> Self {
        let mut set: Vec<NormalizedPrefix> = prefixes
            .iter()
            .map(|prefix| {
                NormalizedPrefix::from_surface(resolver.resolve(prefix.as_ref()).as_path())
            })
            .collect();
        set.sort();
        set.dedup();
        Self(set)
    }

    /// Accumulate two sets without intersecting — the union a deny
    /// region wants, since denies are sticky across layers.
    pub fn union(mut self, other: Self) -> Self {
        self.0.extend(other.0);
        self.0.sort();
        self.0.dedup();
        self
    }

    /// The surface forms, sorted and unique — what a consumer emits.
    /// Re-minted through [`NormalizedPrefix::from_surface`] so the
    /// OS-projection `FsPolicy` carries the same normal form the grant
    /// side does.
    pub fn surface(&self) -> Vec<NormalizedPrefix> {
        let mut out: Vec<String> = self.0.iter().map(|p| p.as_str().to_string()).collect();
        out.sort();
        out.dedup();
        out.into_iter()
            .map(NormalizedPrefix::from_surface)
            .collect()
    }
}

/// Intersection: a prefix survives iff some prefix on each side covers
/// it, and the deeper prefix of each overlapping pair is kept — see
/// [`meet_prefixes`].
impl Meet for PrefixSet {
    fn meet(self, other: Self) -> Self {
        let mut out = meet_prefixes(&self.0, &other.0);
        out.sort();
        out.dedup();
        crate::dbg_trace!(
            "grant-prefix",
            "meet: {:?} ∩ {:?} = {:?}",
            self.0,
            other.0,
            out
        );
        Self(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::resolved::Namespace;

    /// A synthetic prefix with an explicit `surface`/`resolved` pair —
    /// the shape a real symlink would produce, without touching disk.
    fn p(surface: &str, resolved: &str) -> NormalizedPrefix {
        NormalizedPrefix::for_test(surface, resolved, Namespace::Host)
    }

    /// A prefix whose surface and resolved forms coincide — the ordinary
    /// (no-symlink) case.
    fn lit(s: &str) -> NormalizedPrefix {
        p(s, s)
    }

    fn set(ps: &[NormalizedPrefix]) -> PrefixSet {
        PrefixSet(ps.to_vec())
    }

    fn surface(set: &PrefixSet) -> Vec<String> {
        set.surface()
            .into_iter()
            .map(NormalizedPrefix::into_string)
            .collect()
    }

    /// The platform normal form of a path literal — `surface()` mints
    /// its output through `NormalizedPrefix::from_surface`, whose
    /// `fold_dots` kernel reconstructs each path with the host separator
    /// (`/a/x` → `\a\x` on Windows).  Expected values pass through the
    /// same kernel so the assertions hold on both Unix and Windows.
    fn np(s: &str) -> String {
        NormalizedPrefix::from_surface(s).into_string()
    }

    #[test]
    fn meet_keeps_the_deeper_prefix_of_each_overlapping_pair() {
        // {/a,/b} ∩ {/a/x,/c} admits only /a/x (covered by /a on the left, itself on the right).
        assert_eq!(
            surface(&set(&[lit("/a"), lit("/b")]).meet(set(&[lit("/a/x"), lit("/c")]))),
            vec![np("/a/x")]
        );
    }

    #[test]
    fn meet_is_idempotent() {
        let a = set(&[lit("/a"), lit("/a/b"), lit("/c")]);
        assert_eq!(a.clone().meet(a.clone()), a);
    }

    #[test]
    fn meet_is_commutative() {
        let a = set(&[lit("/a"), lit("/b/c")]);
        let b = set(&[lit("/a/x"), lit("/b")]);
        assert_eq!(a.clone().meet(b.clone()), b.meet(a));
    }

    #[test]
    fn union_accumulates_and_dedups() {
        assert_eq!(
            surface(&set(&[lit("/a"), lit("/b")]).union(set(&[lit("/b"), lit("/c")]))),
            vec![np("/a"), np("/b"), np("/c")]
        );
    }

    /// Security regression.  A deeper grant that *lexically* nests under
    /// a shallower ceiling but resolves — through a symlink — outside it
    /// must not survive the intersection.  If it did, the surviving
    /// prefix would reach the OS sandbox and a spawned child could read
    /// the link's target (`bwrap --bind` follows the source symlink;
    /// Seatbelt matches lexically).  Judging overlap on the *resolved*
    /// form is what closes this — the whole reason `NormalizedPrefix`
    /// carries a resolved form distinct from its surface form.
    ///
    /// Over frozen (synthetic) data rather than a real symlink tree: the
    /// meet's fail-closed property is what this test pins, and minting
    /// is the only place that ever consulted a disk — see the module
    /// doc.  `/a/link` lexically nests under the `/a` ceiling but is
    /// given a `resolved` form (`/x`) that does not.
    #[test]
    fn symlinked_grant_cannot_escape_a_shallower_ceiling() {
        let base = set(&[lit("/a")]);
        let inner = set(&[p("/a/link", "/x")]);
        assert!(
            base.meet(inner).surface().is_empty(),
            "a symlinked grant escaping the ceiling must collapse to the empty (fail-closed) meet"
        );
    }

    /// Positive control: a deeper grant that genuinely nests inside the
    /// ceiling survives.  The resolved-form meet *narrows*; it does not
    /// blanket-deny — so the regression above is catching the symlink,
    /// not just an always-empty intersection.
    #[test]
    fn legitimate_nesting_survives_the_meet() {
        let base = set(&[lit("/a")]);
        let inner = set(&[lit("/a/sub")]);
        assert_eq!(
            surface(&base.meet(inner)),
            vec![np("/a/sub")],
            "a genuinely-nested deeper grant must survive the intersection"
        );
    }

    /// `PrefixSet::meet` folds over `path_within_str`
    /// ([`super::lex::path_within`]), whose Windows-identity branch —
    /// case-insensitive, separator-unifying comparison — only fires
    /// under a genuine `cfg!(windows)` build (the real platform gate
    /// lives at `path_within`, per its own doc comment).  So, like the
    /// macOS-only alias tests above, this is gated on `cfg(windows)`
    /// rather than run host-independently: a grant on `C:\work` must
    /// still admit `c:/WORK/sub` (same drive, different case and
    /// separator spelling) once folded through the composed meet, not
    /// just through `starts_with_identity` in isolation (pinned
    /// already in `lex`'s own tests).
    #[cfg(windows)]
    #[test]
    fn meet_admits_windows_case_and_separator_variant() {
        let result = set(&[lit(r"C:\work")]).meet(set(&[lit("c:/WORK/sub")]));
        assert!(
            !result.surface().is_empty(),
            "a grant on C:\\work must admit c:/WORK/sub through the composed meet"
        );
    }

    /// Same property, through a `\\?\`-verbatim spelling — what
    /// `std::fs::canonicalize` returns on Windows — so a grant
    /// resolved through canonicalisation still meets a candidate that
    /// was never canonicalized.
    #[cfg(windows)]
    #[test]
    fn meet_admits_windows_verbatim_prefix_variant() {
        let result = set(&[lit(r"C:\work")]).meet(set(&[lit(r"\\?\C:\work\sub")]));
        assert!(
            !result.surface().is_empty(),
            "a grant on C:\\work must admit \\\\?\\C:\\work\\sub through the composed meet"
        );
    }
}
