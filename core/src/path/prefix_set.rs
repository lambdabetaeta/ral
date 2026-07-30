//! Union and intersection over sets of grant prefixes.
//!
//! [`meet_prefixes`] is the containment kernel [`PrefixSet::meet`] and every
//! lattice meet in `types::capability` share; it keys overlap on the
//! `(namespace, resolved)` form each [`NormalizedPrefix`] froze at mint
//! time, so composing two policies is pure.  The disk is consulted only
//! where a prefix is minted afresh — [`PrefixSet::resolve`] and
//! [`PrefixSet::surface`], both of which re-freeze through
//! [`NormalizedPrefix::from_surface`] — and that is rendering for the
//! sandbox projection, not composition.

use super::lex::{path_within, path_within_str};
use super::resolved::NormalizedPrefix;
use super::resolver::Resolver;
use crate::types::Meet;
use std::path::Path;

/// The one containment judgment: does `a` cover `b`?  Same namespace, `b`'s
/// resolved form within `a`'s — never the surface spelling.
pub fn covers(a: &NormalizedPrefix, b: &NormalizedPrefix) -> bool {
    a.namespace() == b.namespace() && path_within_str(b.resolved(), a.resolved())
}

/// Intersect two prefix lists: the deeper prefix of each overlapping pair
/// survives.  Unsorted and possibly duplicated; callers dedup.
pub fn meet_prefixes(a: &[NormalizedPrefix], b: &[NormalizedPrefix]) -> Vec<NormalizedPrefix> {
    let is_covered =
        |x: &NormalizedPrefix, others: &[NormalizedPrefix]| others.iter().any(|o| covers(o, x));
    a.iter()
        .filter(|x| is_covered(x, b))
        .cloned()
        .chain(b.iter().filter(|y| is_covered(y, a)).cloned())
        .collect()
}

/// A sorted, deduplicated set of [`NormalizedPrefix`]es.  `Default` is the
/// empty set, the identity for [`union`](PrefixSet::union).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrefixSet(Vec<NormalizedPrefix>);

impl PrefixSet {
    /// Freeze each prefix afresh against the live filesystem — sigil/`~`
    /// expansion, `.`/`..` folding, then symlink-following.  Takes the
    /// surface spelling, so an already-frozen [`NormalizedPrefix`] passes
    /// through unchanged but for a re-resolution of its symlinks.
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

    /// Accumulate without intersecting — what a deny region wants, since
    /// denies are sticky across layers.
    pub fn union(mut self, other: Self) -> Self {
        self.0.extend(other.0);
        self.0.sort();
        self.0.dedup();
        self
    }

    /// The deepest prefix whose region contains `path`, if any — the
    /// containment question the runtime gate asks, decided against the same
    /// `resolved` forms [`covers`] keys on and through the same alias-aware
    /// [`path_within`].
    ///
    /// Deepest by [`identity_depth`](super::lex::identity_depth), as
    /// `capability::exec::longest_dir_match` ranks directories: the answer
    /// names the narrowest authority the path fell under, which is what the
    /// audit record wants, and a set that has been through
    /// [`meet`](Meet::meet) has no order left to prefer instead.
    pub fn covering(&self, path: &Path) -> Option<&NormalizedPrefix> {
        self.0
            .iter()
            .filter(|prefix| path_within(path, prefix.resolved_path()))
            .max_by_key(|prefix| super::lex::identity_depth(prefix.resolved(), cfg!(windows)))
    }

    /// The surface forms, sorted and unique, re-minted through
    /// [`NormalizedPrefix::from_surface`] so the rendered `FsPolicy` carries
    /// the same normal form the grant side does.
    pub fn surface(&self) -> Vec<NormalizedPrefix> {
        let mut out: Vec<String> = self.0.iter().map(|p| p.as_str().to_string()).collect();
        out.sort();
        out.dedup();
        out.into_iter()
            .map(NormalizedPrefix::from_surface)
            .collect()
    }
}

/// Intersection, via [`meet_prefixes`].
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
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures build the access-side path from a literal already in normal form"
)]
mod tests {
    use super::*;
    use crate::path::resolved::Namespace;

    /// A divergent `surface`/`resolved` pair — what a symlink freezes to,
    /// without touching disk.
    fn p(surface: &str, resolved: &str) -> NormalizedPrefix {
        NormalizedPrefix::for_test(surface, resolved, Namespace::Host)
    }

    /// The ordinary, no-symlink case: both forms coincide.
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

    /// An expected literal folded through the same kernel `surface()` mints
    /// with, which rebuilds paths with the host separator (`/a/x` → `\a\x`
    /// on Windows), so the assertions hold on both hosts.
    fn np(s: &str) -> String {
        NormalizedPrefix::from_surface(s).into_string()
    }

    #[test]
    fn meet_keeps_the_deeper_prefix_of_each_overlapping_pair() {
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

    /// Security regression.  A grant that lexically nests under a shallower
    /// ceiling but resolves outside it through a symlink must not survive
    /// the meet: the survivor would reach the OS sandbox, where `bwrap
    /// --bind` follows the source symlink and Seatbelt matches lexically, so
    /// a spawned child could read the link's target.  Keying overlap on the
    /// resolved form is what closes this.
    #[test]
    fn symlinked_grant_cannot_escape_a_shallower_ceiling() {
        let base = set(&[lit("/a")]);
        let inner = set(&[p("/a/link", "/x")]);
        assert!(
            base.meet(inner).surface().is_empty(),
            "a symlinked grant escaping the ceiling must collapse to the empty (fail-closed) meet"
        );
    }

    /// Positive control: the resolved-form meet narrows, it does not
    /// blanket-deny, so the regression above is catching the symlink and not
    /// an always-empty intersection.
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

    /// `covering` answers the question the runtime gate asks, so it decides
    /// on the *resolved* form: a prefix that spells as an ancestor of the
    /// access path but resolves elsewhere covers nothing.
    #[test]
    fn covering_matches_the_resolved_form_not_the_surface() {
        let s = set(&[p("/a", "/elsewhere")]);
        assert!(s.covering(Path::new("/a/file")).is_none());
        assert_eq!(
            set(&[p("/link", "/a")]).covering(Path::new("/a/file")),
            Some(&p("/link", "/a")),
            "a prefix resolving onto the access path covers it whatever it spells"
        );
    }

    /// Nested prefixes both cover; the answer is the narrowest authority the
    /// path fell under, which is what the gate's audit record reports.
    #[test]
    fn covering_returns_the_deepest_match() {
        let s = set(&[lit("/a"), lit("/a/b"), lit("/a/b/c"), lit("/other")]);
        assert_eq!(s.covering(Path::new("/a/b/x")), Some(&lit("/a/b")));
        assert_eq!(s.covering(Path::new("/a/y")), Some(&lit("/a")));
        assert_eq!(s.covering(Path::new("/z")), None);
    }

    /// The empty set is the fail-closed meet, so it must cover nothing —
    /// this is what turns a collapsed intersection into a gate denial.
    #[test]
    fn the_empty_set_covers_nothing() {
        assert_eq!(PrefixSet::default().covering(Path::new("/a")), None);
    }

    /// Depth is counted in components of the alias-folded form, as
    /// `capability::exec::longest_dir_match` counts it: `/private/tmp` is an
    /// alias of the one-deep `/tmp` yet spells longer, so a character count
    /// would rank it above the genuinely deeper `/tmp/a`.
    #[cfg(target_os = "macos")]
    #[test]
    fn covering_ranks_depth_by_components_not_characters() {
        let s = set(&[lit("/private/tmp"), lit("/tmp/a")]);
        assert_eq!(s.covering(Path::new("/tmp/a/f")), Some(&lit("/tmp/a")));
    }

    /// Gated because the case- and separator-folding branch of
    /// `path_within` fires only under a real `cfg!(windows)` build.  `lex`
    /// pins that comparison in isolation; this pins it through the composed
    /// meet.
    #[cfg(windows)]
    #[test]
    fn meet_admits_windows_case_and_separator_variant() {
        let result = set(&[lit(r"C:\work")]).meet(set(&[lit("c:/WORK/sub")]));
        assert!(
            !result.surface().is_empty(),
            "a grant on C:\\work must admit c:/WORK/sub through the composed meet"
        );
    }

    /// Same, through the `\\?\`-verbatim spelling `std::fs::canonicalize`
    /// returns on Windows, so a canonicalised grant still meets a candidate
    /// that was never canonicalised.
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
